use anyhow::Result;
use futures::future::try_join_all;
use tracing::{debug, warn};

use crate::provider::{LlmProvider, LlmRole};
use crate::smt::{extract_all_formulas, extract_single_formula};
use crate::states::{CodePiece, InsistState, PieceFormula, VerifiedPiece, WaitForGeneration};

pub fn role_for_iteration(iteration: usize) -> LlmRole {
    if iteration == 0 {
        LlmRole::Formalizer
    } else {
        LlmRole::Fixer
    }
}

pub async fn execute(
    state: &WaitForGeneration,
    llm: &dyn LlmProvider,
) -> Result<Vec<PieceFormula>> {
    let role = role_for_iteration(state.iteration);

    if let InsistState::Insisting { ref last_response, .. } = &state.insist {
        return generate_insist(state, llm, role, last_response).await;
    }

    assert!(!state.pieces.is_empty(), "must have pieces");

    let futures: Vec<_> = state
        .pieces
        .iter()
        .map(|piece| {
            debug!(piece_id = piece.id, label = %piece.label, "dispatching generation for piece");
            generate_one_formula(piece, &state.input_content, &state.verified, llm, role)
        })
        .collect();
    let formulas: Vec<String> = try_join_all(futures).await?;
    debug!(count = formulas.len(), "generation complete for all pieces");

    let results: Vec<PieceFormula> = state
        .pieces
        .iter()
        .zip(formulas)
        .map(|(piece, formula)| {
            debug!(piece_id = piece.id, label = %piece.label, "collected formula for piece");
            PieceFormula {
                piece: piece.clone(),
                formula,
            }
        })
        .collect();
    Ok(results)
}

async fn generate_one_formula(
    piece: &CodePiece,
    input_content: &str,
    verified: &[VerifiedPiece],
    llm: &dyn LlmProvider,
    role: LlmRole,
) -> Result<String> {
    let messages = build_single_piece_messages(piece, input_content, verified);
    let response = llm.chat(role, messages, Some(piece)).await?;
    let formula = extract_single_formula(&response);
    debug!(piece_id = piece.id, label = %piece.label, bytes = formula.len(), "extracted formula for piece");
    Ok(formula)
}

async fn generate_insist(
    state: &WaitForGeneration,
    llm: &dyn LlmProvider,
    role: LlmRole,
    last_response: &str,
) -> Result<Vec<PieceFormula>> {
    let pieces_text: String = state
        .pieces
        .iter()
        .enumerate()
        .fold(String::new(), |mut s, (i, piece)| {
            let _ = std::fmt::Write::write_fmt(
                &mut s,
                format_args!(
                    "Piece {}: {} #{} (BEFORE: {}, AFTER: {})\n",
                    i + 1, piece.label, piece.id, piece.before, piece.after,
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

    let response = llm.chat(role, messages, None).await?;
    let mut formulas = extract_all_formulas(&response);

    if formulas.len() != state.pieces.len() {
        warn!(
            expected = state.pieces.len(),
            got = formulas.len(),
            "insist generation produced wrong number of formulas"
        );
        return Ok(Vec::new());
    }

    let results: Vec<PieceFormula> = state
        .pieces
        .iter()
        .zip(formulas.drain(..))
        .map(|(piece, formula)| {
            debug!(piece_id = piece.id, label = %piece.label, "collected insist formula for piece");
            PieceFormula {
                piece: piece.clone(),
                formula,
            }
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

    messages.push(crate::llm::system_message(&format!(
        "Piece ID: {id}\n\
         You are an expert in formal verification. Generate ONE complete SMT-LIB2 formula \
         to verify equivalence of this BEFORE/AFTER pair. \
         \nOutput exactly ONE formula in a single ```smt2 code block.\n\
         The formula must be complete (include set-logic, declarations, assertions, check-sat).\n\
         If the before/after are equivalent, the formula should be unsatisfiable.\n\
         If the formula is satisfiable, the code is NOT equivalent.",
        id = piece.id,
    )));

    let mut content = format!(
        "Verify this piece:\nLabel: {}\nBEFORE:\n{}\nAFTER:\n{}\n",
        piece.label, piece.before, piece.after,
    );
    if !input_content.is_empty() {
        content = format!("Original refactoring context:\n\n{}\n\n{}", input_content, content);
    }
    if !verified.is_empty() {
        content.push_str("Already verified pieces:\n");
        for v in verified {
            content.push_str(&format!("  {}: {:?}\n", v.piece.label, v.outcome));
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
        label = piece.label,
        before = piece.before,
        after = piece.after,
    );

    vec![
        crate::llm::system_message(
            "You are an expert in formal verification. Fix the following SMT formula so \
             it correctly checks equivalence of this specific BEFORE/AFTER pair. \
             Do NOT output any explanation. \
             Output ONLY the fixed formula in a single ```smt2 code block.",
        ),
        crate::llm::user_message(&prompt),
    ]
}

pub fn build_retry_insist_messages(
    piece: &CodePiece,
    feedback: &str,
    last_response: &str,
    input_content: &str,
) -> Vec<crate::llm::Message> {
    vec![
        crate::llm::system_message(
            "You MUST output exactly one SMT-LIB2 formula in a single ```smt2 code block. \
             Do NOT include any explanations.",
        ),
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
            label = piece.label,
            before = piece.before,
            after = piece.after,
        )),
    ]
}