use anyhow::Result;
use tracing::{debug, warn};

use crate::consts::{JUDGE_REASONABLE, MAX_JUDGE_ATTEMPTS};
use crate::llm;
use crate::provider::{DynLlmProvider, LlmRequest, LlmRole};
use crate::smt::SolverResult;
use crate::states::{CodePiece, JudgeVerdict};

pub async fn execute(
    piece: &CodePiece,
    formula: &str,
    solver_result: &SolverResult,
    llm: &DynLlmProvider,
    input_content: &str,
) -> Result<JudgeVerdict> {
    let mut attempts = 0;
    let mut last_response = String::new();

    assert!(
        !piece.before().is_empty() && !piece.after().is_empty(),
        "piece {} #{} must have non-empty BEFORE and AFTER",
        piece.label(),
        piece.id(),
    );

    loop {
        attempts += 1;
        if attempts >= MAX_JUDGE_ATTEMPTS {
            anyhow::bail!("Judge failed to give a clear verdict after {MAX_JUDGE_ATTEMPTS} attempts");
        }

        debug!(attempt = attempts, piece_id = piece.id(), label = %piece.label(), "asking judge for verdict");

        let prompt = if attempts == 1 {
            format!(
                "Original refactoring context:\n\n{ctx}\n\n\
                 Piece to verify: {label}\n\
                 BEFORE:\n{before}\n\
                 AFTER:\n{after}\n\n\
                 Formula checking this piece:\n\n{formula}\n\n\
                 Solver result:\n{}\n\n\
                 Is this formula correctly checking equivalence of this specific BEFORE/AFTER pair? \
                 Answer ONLY with the single word REASONABLE if yes. \
                 Otherwise explain what is wrong with the formula.",
                solver_result.stdout,
                ctx = input_content,
                label = piece.label(),
                before = piece.before(),
                after = piece.after(),
            )
        } else {
            format!(
                "Original refactoring context:\n\n{ctx}\n\n\
                 Piece to verify: {label}\n\
                 BEFORE:\n{before}\n\
                 AFTER:\n{after}\n\n\
                 Formula checking this piece:\n\n{formula}\n\n\
                 Solver result:\n{}\n\n\
                 Your previous answer was: '{last_response}'. \
                 You MUST answer ONLY with REASONABLE or explain what is wrong.",
                solver_result.stdout,
                ctx = input_content,
                label = piece.label(),
                before = piece.before(),
                after = piece.after(),
            )
        };

        let messages = vec![
            llm::system_message(
                "You are a judge evaluating an SMT-based equivalence check. \
                 Your job is to verify that the formula VALIDLY ENCODES the semantics of the \
                 BEFORE and AFTER code — not to judge whether the formula itself is tautological. \
                 \n\nIf the formula correctly captures the behavior of both BEFORE and AFTER code, \
                 answer ONLY with the single word REASONABLE. \
                 \n\nIf the formalization DEVIATES from the semantics (wrong encoding, missing \
                 behaviors, incorrect assertions), explain exactly where it deviates: \
                 which part of the BEFORE or AFTER code is misrepresented, \
                 and what is wrong with the formalization. Provide your explanation concisely. \
                 \n\nFor loops or recursive functions, assume that checking single executions of the loop body, \
                 or single code flows through recursive calls with recursion arguments proven to be equal \
                 (potentially under some introduced loop invariant), is sufficient.",
            ),
            llm::user_message(&prompt),
        ];

        let response = llm.invoke(LlmRequest { role: LlmRole::Judge, messages, piece_id: Some(piece.id()) }).await?;
        let trimmed = response.trim().to_string();
        let upper = trimmed.to_uppercase();

        if upper.starts_with(JUDGE_REASONABLE) {
            return Ok(JudgeVerdict::Reasonable);
        }

        if !trimmed.is_empty() {
            return Ok(JudgeVerdict::Retry(trimmed));
        }

        warn!(response = %response, "judge gave unclear answer, insisting");
        last_response = response;
    }
}