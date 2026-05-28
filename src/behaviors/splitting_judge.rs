use anyhow::Result;
use tracing::{debug, warn};

use crate::consts::{JUDGE_REASONABLE, MAX_JUDGE_ATTEMPTS};
use crate::llm;
use crate::provider::{LlmProvider, LlmRole};
use crate::states::WaitForSplittingJudge;

pub enum SplittingJudgeVerdict {
    Accept,
    Retry(String),
}

pub async fn execute(
    state: &WaitForSplittingJudge,
    llm: &dyn LlmProvider,
) -> Result<SplittingJudgeVerdict> {
    let mut attempts = 0;
    let mut last_response = String::new();

    let pieces_text: String = state
        .pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| {
            format!(
                "Piece {}: {} (BEFORE: {}, AFTER: {})",
                i + 1,
                piece.label(),
                piece.before(),
                piece.after(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    loop {
        attempts += 1;
        if attempts >= MAX_JUDGE_ATTEMPTS {
            anyhow::bail!("Splitting judge failed to give a clear verdict after {MAX_JUDGE_ATTEMPTS} attempts");
        }

        debug!(attempt = attempts, "asking splitting judge for verdict");

        let prompt = if attempts == 1 {
            format!(
                "Original refactoring context:\n\n{ctx}\n\n\
                 The splitter produced these pieces:\n\n{pieces}\n\n\
                 Is this a good decomposition for SMT-based equivalence checking? \
                 Each piece should be independently verifiable, not too large \
                 (to avoid solver timeouts), and each BEFORE must have a matching AFTER. \
                 Answer ONLY with the single word REASONABLE if the split is good. \
                 Otherwise explain what is wrong and suggest how to improve it.",
                ctx = state.input_content,
                pieces = pieces_text,
            )
        } else {
            format!(
                "Original refactoring context:\n\n{ctx}\n\n\
                 The splitter produced these pieces:\n\n{pieces}\n\n\
                 Your previous answer was: '{last_response}'. \
                 You MUST answer ONLY with REASONABLE or explain what is wrong.",
                ctx = state.input_content,
                pieces = pieces_text,
            )
        };

        let messages = vec![
            llm::system_message(
                "You are a judge evaluating code decompositions for SMT-based equivalence \
                 verification. Your job is to verify that the decomposition is sound: \
                 each piece should be independently verifiable, not so large that an SMT \
                 solver would time out, and each BEFORE must have a matching AFTER. \
                 \n\nIf the decomposition is good, answer ONLY with the single word REASONABLE. \
                 \n\nIf the decomposition has problems (pieces too large, missing code, \
                 mismatched BEFORE/AFTER, overlapping pieces), explain what is wrong concisely.",
            ),
            llm::user_message(&prompt),
        ];

        let response = llm.chat(LlmRole::SplittingJudge, messages, None).await?;
        let trimmed = response.trim().to_string();
        let upper = trimmed.to_uppercase();

        if upper.starts_with(JUDGE_REASONABLE) {
            return Ok(SplittingJudgeVerdict::Accept);
        }

        if !trimmed.is_empty() {
            return Ok(SplittingJudgeVerdict::Retry(trimmed));
        }

        warn!(response = %response, "splitting judge gave unclear answer, insisting");
        last_response = response;
    }
}
