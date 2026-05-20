use anyhow::Result;
use futures::future::try_join_all;

use crate::provider::{LlmProvider, LlmRole};
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
    llm: &dyn LlmProvider,
) -> Result<Vec<String>> {
    let role = role_for_iteration(state.iteration);

    if let InsistState::Insisting { ref last_response, .. } = &state.insist {
        let messages = vec![
            crate::llm::system_message(
                "You MUST output at least ONE SMT-LIB2 formula. \
                 Put each formula in a separate ```smt2 code block. \
                 Each formula must be complete with set-logic, declarations, assertions, and check-sat.",
            ),
            crate::llm::user_message(&format!(
                "Your previous response did not contain any valid SMT formula. \
                 Here was your previous response:\n\n{last_response}\n\n\
                 Please try again. Output at least ONE valid SMT-LIB2 formula in a ```smt2 code block.",
            )),
        ];
        let response = llm.chat(role, messages).await?;
        return Ok(vec![response]);
    }

    if !state.pieces.is_empty() {
        let futures: Vec<_> = state
            .pieces
            .iter()
            .map(|piece| {
                let messages =
                    build_single_piece_messages(piece, &state.input_content, &state.verified);
                llm.chat(role, messages)
            })
            .collect();
        try_join_all(futures).await
    } else {
        let messages = crate::agent::build_generation_messages(
            &state.input_content,
            &state.verified,
            &state.open,
        );
        let response = llm.chat(role, messages).await?;
        Ok(vec![response])
    }
}

fn build_single_piece_messages(
    piece: &CodePiece,
    input_content: &str,
    verified: &[VerifiedPiece],
) -> Vec<crate::llm::Message> {
    let mut messages = Vec::new();

    messages.push(crate::llm::system_message(
        "You are an expert in formal verification. Generate ONE complete SMT-LIB2 formula \
         to verify equivalence of this BEFORE/AFTER pair. \
         \n\nRules:\n\
         - Output exactly ONE formula in a single ```smt2 code block.\n\
         - The formula must be complete (include set-logic, declarations, assertions, check-sat).\n\
         - If the before/after are equivalent, the formula should be unsatisfiable.\n\
         - If the formula is satisfiable, the code is NOT equivalent.",
    ));

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
            content.push_str(&format!("  {}: {:?}\n", v.piece_label, v.outcome));
        }
        content.push('\n');
    }

    messages.push(crate::llm::user_message(&content));
    messages
}

// Keep old builder for backward compat (no pieces case)
pub fn build_retry_messages(
    piece: &CodePiece,
    formula: &str,
    feedback: &str,
    solver_stdout: &str,
    solver_stderr: &str,
) -> Vec<crate::llm::Message> {
    let prompt = format!(
        "Piece to verify: {label}\n\
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
) -> Vec<crate::llm::Message> {
    vec![
        crate::llm::system_message(
            "You MUST output exactly one SMT-LIB2 formula in a single ```smt2 code block. \
             Do NOT include any explanations.",
        ),
        crate::llm::user_message(&format!(
            "Your previous response contained no valid SMT formula.\n\
             Here it was:\n\n{last_response}\n\n\
             Piece to fix: {label}\n\
             BEFORE:\n{before}\n\
             AFTER:\n{after}\n\n\
             Feedback: {feedback}\n\n\
             Try again. ONE complete formula in a ```smt2 code block.",
            label = piece.label,
            before = piece.before,
            after = piece.after,
        )),
    ]
}