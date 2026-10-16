use std::sync::Arc;

use anyhow::Result;
use refactor_check_core::checkpoint::Handle;
use tracing::{debug, warn};

use crate::phase::PiecePhase;
use crate::piece_manager::PieceManager;
use crate::provider::{DynLlmProvider, LlmRequest, LlmRole};
use crate::smt::{extract_all_formulas, extract_single_formula};
use crate::states::{CodePiece, GenerationSlot, VerifiedPiece, WaitForGeneration};

pub fn role_for_iteration(iteration: usize) -> LlmRole {
    if iteration == 0 {
        LlmRole::Formalizer
    } else {
        LlmRole::Fixer
    }
}

fn phase_for_role(role: LlmRole) -> PiecePhase {
    if role == LlmRole::Formalizer { PiecePhase::Forming } else { PiecePhase::Fixing }
}

/// What the per-piece workers of one generation fan-out share, copied out of
/// the stage before the fan-out borrows it.
pub struct GenerationJob {
    input_content: Arc<String>,
    verified_summary: String,
    role: LlmRole,
}

impl GenerationJob {
    /// # Panics
    ///
    /// Panics if the stage has no pieces.
    pub fn new(state: &WaitForGeneration) -> Self {
        assert!(!state.pieces.is_empty(), "must have pieces");
        Self {
            input_content: Arc::clone(&state.input_content),
            verified_summary: verified_summary(&state.verified),
            role: role_for_iteration(state.iteration),
        }
    }

    /// Generates the slot's formula, unless an earlier run already stored it.
    ///
    /// # Panics
    ///
    /// Panics via [`CodePiece::with_ctx`] if the piece's context was already
    /// taken and not restored.
    pub fn run(
        &self,
        mut slot: Handle<'_, GenerationSlot>,
        llm: &DynLlmProvider,
        pm: &dyn PieceManager,
    ) -> Result<()> {
        if slot.formula.is_some() {
            return Ok(());
        }
        slot.piece.with_ctx(|ctx| pm.enter_generation(ctx, phase_for_role(self.role)));
        debug!(ctx = %slot.piece.ctx_display(), label = %slot.piece.label(), "dispatching generation for piece");
        let formula = generate_one_formula(&slot.piece, &self.input_content, &self.verified_summary, llm, self.role)?;
        slot.formula = Some(formula);
        Ok(())
    }
}

/// The formulas the generation fan-out stored, or none if any piece lacks a
/// valid one (the caller then insists).
pub fn collect(state: &WaitForGeneration) -> Vec<String> {
    let formulas: Vec<String> = state
        .pieces
        .iter()
        .map(|slot| slot.formula.clone().unwrap_or_default())
        .collect();
    if formulas.iter().any(|f| f.is_empty()) {
        warn!("one or more pieces produced no valid formula, entering insist loop");
        return Vec::new();
    }
    debug!(count = formulas.len(), "generation complete for all pieces");
    formulas
}

fn generate_one_formula(
    piece: &CodePiece,
    input_content: &str,
    verified_summary: &str,
    llm: &DynLlmProvider,
    role: LlmRole,
) -> Result<String> {
    let messages = build_single_piece_messages(piece, input_content, verified_summary);
    let ctx = piece.take_context().map_err(anyhow::Error::msg)?;
    let resp = llm.invoke(LlmRequest { role, messages, context_id: ctx })?;
    piece.restore_context(resp.context_id);
    let response = resp.value;
    let formula = extract_single_formula(&response);
    debug!(ctx = %piece.ctx_display(), label = %piece.label(), bytes = formula.len(), "extracted formula for piece");
    Ok(formula)
}

/// Asks for all of the stage's formulas in one response, after a fan-out left
/// some piece without a valid one.
///
/// # Panics
///
/// Panics via [`CodePiece::with_ctx`] if a piece's context was already taken
/// and not restored, or if the stage has no pieces.
pub fn generate_insist(
    state: &WaitForGeneration,
    llm: &DynLlmProvider,
    pm: &dyn PieceManager,
    last_response: &str,
) -> Result<Vec<String>> {
    let role = role_for_iteration(state.iteration);
    for slot in &state.pieces {
        slot.piece.with_ctx(|ctx| pm.enter_generation(ctx, phase_for_role(role)));
    }

    let pieces_text: String = state
        .pieces
        .iter()
        .enumerate()
        .fold(String::new(), |mut s, (i, slot)| {
            let piece = &slot.piece;
            let _ = std::fmt::Write::write_fmt(
                &mut s,
                format_args!(
                    "Piece {}: {} {} (BEFORE: {}, AFTER: {})\n",
                    i + 1, piece.label(), piece.ctx_display(), piece.before(), piece.after(),
                ),
            );
            s
        });

    let messages = vec![
        crate::llm::system_message(&format!(
            "You MUST output exactly ONE SMT-LIB2 formula per piece in a ```smt2 code block. \
             Output exactly {n} formulas, one per piece, in order.",
            n = state.pieces.len(),
        )),
        crate::llm::user_message(&format!(
            "Original refactoring context:\n\n{ctx}\n\n\
             {pieces}\n\
             Your previous response did not contain valid formulas. \
             Here was your previous response:\n\n{last_response}\n\n\
             Please try again. Output exactly {n} formulas, one per piece, \
             each in a ```smt2 code block.",
            n = state.pieces.len(),
            ctx = state.input_content,
            pieces = pieces_text,
        )),
    ];

    let first = &state.pieces[0].piece;
    let ctx = first.take_context().map_err(anyhow::Error::msg)?;
    let resp = llm.invoke(LlmRequest { role, messages, context_id: ctx })?;
    first.restore_context(resp.context_id);
    let response = resp.value;
    let mut formulas = extract_all_formulas(&response);

    if formulas.len() != state.pieces.len() {
        warn!(
            expected = state.pieces.len(),
            got = formulas.len(),
            "insist generation produced wrong number of formulas"
        );
        return Ok(Vec::new());
    }

    let results: Vec<String> = state
        .pieces
        .iter()
        .zip(formulas.drain(..))
        .map(|(slot, formula)| {
            debug!(ctx = %slot.piece.ctx_display(), label = %slot.piece.label(), "collected insist formula for piece");
            formula
        })
        .collect();
    Ok(results)
}

fn verified_summary(verified: &[VerifiedPiece]) -> String {
    let mut summary = String::new();
    if !verified.is_empty() {
        summary.push_str("Already verified pieces:\n");
        for v in verified {
            summary.push_str(&format!("  {}: {:?}\n", v.piece.label(), v.outcome));
        }
        summary.push('\n');
    }
    summary
}

fn build_single_piece_messages(
    piece: &CodePiece,
    input_content: &str,
    verified_summary: &str,
) -> Vec<crate::llm::Message> {
    let mut messages = Vec::new();

    let relation_guidance = if piece.before().contains("/* relation:") || piece.after().contains("/* relation:") {
        "\n\nIf /* relation: ... */ comments appear in the code, they specify how certain \
         states differ between before and after. Verify that the piece satisfies the \
         specified relation. All variables/states not mentioned in a relation comment \
         are assumed to be equivalent between before and after. The conjunction of all \
         piece relations must imply overall equivalence of the full refactoring. \
         If no relation comments are present, verify strict equivalence as before."
    } else {
        ""
    };

    messages.push(crate::llm::system_message(&format!(
        "Piece ID: {id}\n\
         You are an expert in formal verification. Generate ONE complete SMT-LIB2 formula \
         to verify equivalence of this BEFORE/AFTER pair. \
         \nOutput exactly ONE formula in a single ```smt2 code block.\n\
         The formula must be complete (include set-logic, declarations, assertions, check-sat).\n\
         If the before/after are equivalent, the formula should be unsatisfiable.\n\
         If the formula is satisfiable, the code is NOT equivalent.{relation_guidance}",
        id = piece.ctx_display(),
    )));

    let mut content = format!(
        "Verify this piece:\nLabel: {}\nBEFORE:\n{}\nAFTER:\n{}\n",
        piece.label(), piece.before(), piece.after(),
    );
    if !input_content.is_empty() {
        content = format!("Original refactoring context:\n\n{}\n\n{}", input_content, content);
    }
    content.push_str(verified_summary);

    messages.push(crate::llm::user_message(&content));
    messages
}

pub fn build_retry_messages(
    piece: &CodePiece,
    formula: &str,
    feedback: &str,
    solver_stdout: &str,
    solver_stderr: &str,
    input_content: &str,
) -> Vec<crate::llm::Message> {
    let relation_guidance = if piece.before().contains("/* relation:") || piece.after().contains("/* relation:") {
        " If /* relation: ... */ comments are present, encode the specified relation \
         between before and after states, not just strict equivalence. Variables/states \
         not mentioned in the relation are assumed equivalent."
    } else {
        ""
    };

    let prompt = format!(
        "Original refactoring context:\n\n{ctx}\n\n\
         Piece to verify: {label}\n\
         BEFORE:\n{before}\n\
         AFTER:\n{after}\n\n\
         The formula that failed:\n{}\n\n\
         Judge feedback: {}\n\n\
         Solver output: {}\n\n\
         Solver stderr: {}\n\n\
         Please provide ONE corrected SMT-LIB2 formula in a ```smt2 code block.",
        formula,
        feedback,
        solver_stdout,
        solver_stderr,
        ctx = input_content,
        label = piece.label(),
        before = piece.before(),
        after = piece.after(),
    );

    vec![
        crate::llm::system_message(&format!(
            "You are an expert in formal verification. Fix the following SMT formula so \
             it correctly checks equivalence of this specific BEFORE/AFTER pair. \
             Do NOT output any explanation. \
             Output ONLY the fixed formula in a single ```smt2 code block.{relation_guidance}",
        )),
        crate::llm::user_message(&prompt),
    ]
}

pub fn build_retry_insist_messages(
    piece: &CodePiece,
    feedback: &str,
    last_response: &str,
    input_content: &str,
) -> Vec<crate::llm::Message> {
    let relation_guidance = if piece.before().contains("/* relation:") || piece.after().contains("/* relation:") {
        " If /* relation: ... */ comments are present, encode the specified relation \
         between before and after states, not just strict equivalence."
    } else {
        ""
    };

    vec![
        crate::llm::system_message(&format!(
            "You MUST output exactly one SMT-LIB2 formula in a single ```smt2 code block. \
             Do NOT include any explanations.{relation_guidance}",
        )),
        crate::llm::user_message(&format!(
            "Original refactoring context:\n\n{ctx}\n\n\
             Your previous response contained no valid SMT formula.\n\
             Here it was:\n\n{last_response}\n\n\
             Piece to fix: {label}\n\
             BEFORE:\n{before}\n\
             AFTER:\n{after}\n\n\
             Feedback: {feedback}\n\n\
             Try again. ONE complete formula in a ```smt2 code block.",
            ctx = input_content,
            label = piece.label(),
            before = piece.before(),
            after = piece.after(),
        )),
    ]
}