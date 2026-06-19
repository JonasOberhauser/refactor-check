use anyhow::Result;
use refactor_check_core::context_id::ContextId;
use tracing::{debug, warn};

use crate::consts::{JUDGE_REASONABLE, MAX_JUDGE_ATTEMPTS};
use crate::llm;
use crate::provider::{DynLlmProvider, LlmRequest, LlmRole};
use crate::states::WaitForSplittingJudge;

pub enum SplittingJudgeVerdict {
    Accept,
    Retry(String),
}

pub async fn execute(
    state: &WaitForSplittingJudge,
    llm: &DynLlmProvider,
    mut parent_ctx: ContextId,
) -> Result<(SplittingJudgeVerdict, ContextId)> {
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
                 Each piece should be independently verifiable (with any deviations \
                 from equivalence explicitly captured in a /* relation: ... */ comment), \
                 not too large (to avoid solver timeouts), and each BEFORE must have \
                 a matching AFTER. \
                 Check that /* relation: ... */ comments are consistent across adjacent \
                 pieces: if piece 1 has relation R1 and piece 2 has relation R2, then \
                 R1 composed with R2 must imply equivalence for the combined code. \
                 Variables/states not mentioned in a relation comment are assumed equivalent. \
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
                 each piece should be independently verifiable (with any deviations from \
                 equivalence explicitly captured in a /* relation: ... */ comment), \
                 not so large that an SMT solver would time out, and each BEFORE must \
                 have a matching AFTER. Check that /* relation: ... */ comments are \
                 consistent across adjacent pieces: relations must compose so that the \
                 conjunction of all piece relations implies overall equivalence. \
                 Variables/states not mentioned in a relation comment are assumed equivalent. \
                 \n\nIf the decomposition is good, answer ONLY with the single word REASONABLE. \
                 \n\nIf the decomposition has problems (pieces too large, missing code, \
                 mismatched BEFORE/AFTER, overlapping pieces, inconsistent relations), \
                 explain what is wrong concisely.",
            ),
            llm::user_message(&prompt),
        ];

        let resp = llm.invoke(LlmRequest { role: LlmRole::SplittingJudge, messages, context_id: Box::new(parent_ctx) }).await?;
        parent_ctx = *resp.context_id;
        let response = resp.value;
        let trimmed = response.trim().to_string();
        let upper = trimmed.to_uppercase();

        if upper.starts_with(JUDGE_REASONABLE) {
            return Ok((SplittingJudgeVerdict::Accept, parent_ctx));
        }

        if !trimmed.is_empty() {
            return Ok((SplittingJudgeVerdict::Retry(trimmed), parent_ctx));
        }

        warn!(response = %response, "splitting judge gave unclear answer, insisting");
        last_response = response;
    }
}
