use anyhow::Result;
use tracing::{debug, warn};

use crate::consts::{JUDGE_REASONABLE, MAX_JUDGE_ATTEMPTS};
use crate::llm;
use crate::provider::{LlmProvider, LlmRole};
use crate::smt::SolverResult;
use crate::states::{CodePiece, JudgeVerdict};

pub async fn execute(
    piece: &CodePiece,
    formula: &str,
    solver_result: &SolverResult,
    llm: &dyn LlmProvider,
) -> Result<JudgeVerdict> {
    let mut attempts = 0;
    let mut last_response = String::new();

    loop {
        attempts += 1;
        if attempts >= MAX_JUDGE_ATTEMPTS {
            anyhow::bail!("Judge failed to give a clear verdict after {MAX_JUDGE_ATTEMPTS} attempts");
        }

        debug!(attempt = attempts, "asking judge for verdict");

        let prompt = if attempts == 1 {
            format!(
                "Piece to verify: {label}\n\
                 BEFORE:\n{before}\n\
                 AFTER:\n{after}\n\n\
                 Formula checking this piece:\n\n{formula}\n\n\
                 Solver result:\n{}\n\n\
                 Is this formula correctly checking equivalence of this specific BEFORE/AFTER pair? \
                 Answer ONLY with the single word REASONABLE if yes. \
                 Otherwise explain what is wrong with the formula.",
                solver_result.stdout,
                label = piece.label,
                before = piece.before,
                after = piece.after,
            )
        } else {
            format!(
                "Piece to verify: {label}\n\
                 BEFORE:\n{before}\n\
                 AFTER:\n{after}\n\n\
                 Formula checking this piece:\n\n{formula}\n\n\
                 Solver result:\n{}\n\n\
                 Your previous answer was: '{last_response}'. \
                 You MUST answer ONLY with REASONABLE or explain what is wrong.",
                solver_result.stdout,
                label = piece.label,
                before = piece.before,
                after = piece.after,
            )
        };

        let messages = vec![
            llm::system_message(
                "You are a judge evaluating an SMT-based equivalence check. \
                 If the formula correctly and completely checks equivalence of the \
                 specific BEFORE/AFTER pair, answer ONLY with the single word REASONABLE. \
                 If the formula does NOT correctly check equivalence, explain what is wrong: \
                 Does it fail to represent some behavior? Is the encoding wrong? \
                 Are assertions missing important cases? Provide your explanation concisely.",
            ),
            llm::user_message(&prompt),
        ];

        let response = llm.chat(LlmRole::Judge, messages).await?;
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