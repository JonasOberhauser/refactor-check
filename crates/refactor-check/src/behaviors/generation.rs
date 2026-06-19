use anyhow::Result;
use futures::future::try_join_all;
use tracing::{debug, warn};

use crate::phase::PiecePhase;
use crate::piece_manager::PieceManager;
use crate::provider::{DynLlmProvider, LlmRequest, LlmRole};
use crate::smt::{extract_all_formulas, extract_single_formula};
use crate::states::{CodePiece, InsistState, VerifiedPiece, WaitForGeneration};

pub fn role_for_iteration(iteration: usize) -> LlmRole {
    if iteration == 0 {
        LlmRole::Formalizer
    } else {
        LlmRole::Fixer
    }
}

pub async fn execute(
    state: &WaitForGeneration,
    llm: &DynLlmProvider,
    pm: &dyn PieceManager,
) -> Result<Vec<String>> {
    let role = role_for_iteration(state.iteration);
    let new_phase = if role == LlmRole::Formalizer { PiecePhase::Forming } else { PiecePhase::Fixing };
    for piece in &state.pieces {
        piece.with_ctx(|ctx| pm.enter_generation(ctx, new_phase));
    }

    if let InsistState::Insisting { ref last_response, .. } = &state.insist {
        return generate_insist(state, llm, role, last_response).await;
    }

    assert!(!state.pieces.is_empty(), "must have pieces");

    let futures: Vec<_> = state
        .pieces
        .iter()
        .map(|piece| {
            debug!(ctx = %piece.ctx_display(), label = %piece.label(), "dispatching generation for piece");
            generate_one_formula(piece, &state.input_content, &state.verified, llm, role)
        })
        .collect();
    let formulas: Vec<String> = try_join_all(futures).await?;
    if formulas.iter().any(|f| f.is_empty()) {
        warn!("one or more pieces produced no valid formula, entering insist loop");
        return Ok(Vec::new());
    }
    debug!(count = formulas.len(), "generation complete for all pieces");
    Ok(formulas)
}

async fn generate_one_formula(
    piece: &CodePiece,
    input_content: &str,
    verified: &[VerifiedPiece],
    llm: &DynLlmProvider,
    role: LlmRole,
) -> Result<String> {
    let messages = build_single_piece_messages(piece, input_content, verified);
    let ctx = piece.take_context();
    let resp = llm.invoke(LlmRequest { role, messages, context_id: ctx }).await?;
    piece.restore_context(resp.context_id);
    let response = resp.value;
    let formula = extract_single_formula(&response);
    debug!(ctx = %piece.ctx_display(), label = %piece.label(), bytes = formula.len(), "extracted formula for piece");
    Ok(formula)
}

async fn generate_insist(
    state: &WaitForGeneration,
    llm: &DynLlmProvider,
    role: LlmRole,
    last_response: &str,
) -> Result<Vec<String>> {
    let pieces_text: String = state
        .pieces
        .iter()
        .enumerate()
        .fold(String::new(), |mut s, (i, piece)| {
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

    let ctx = state.pieces[0].take_context();
    let resp = llm.invoke(LlmRequest { role, messages, context_id: ctx }).await?;
    state.pieces[0].restore_context(resp.context_id);
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
        .map(|(piece, formula)| {
            debug!(ctx = %piece.ctx_display(), label = %piece.label(), "collected insist formula for piece");
            formula
        })
        .collect();
    Ok(results)
}

fn build_single_piece_messages(
    piece: &CodePiece,
    input_content: &str,
    verified: &[VerifiedPiece],
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
    if !verified.is_empty() {
        content.push_str("Already verified pieces:\n");
        for v in verified {
            content.push_str(&format!("  {}: {:?}\n", v.piece.label(), v.outcome));
        }
        content.push('\n');
    }

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