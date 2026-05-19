use anyhow::Result;
use tracing::info;

use crate::agent::build_generation_messages;
use crate::agent::GenerationContext;
use crate::consts::MAX_INSIST_ATTEMPTS;
use crate::provider::{LlmProvider, LlmRole};
use crate::smt::extract_all_formulas;
use crate::states::WaitForGeneration;

pub async fn execute(
    state: &WaitForGeneration,
    llm: &dyn LlmProvider,
) -> Result<String> {
    let mut response = call_generation(state, llm).await?;
    let mut formula_count = extract_all_formulas(&response).len();

    if formula_count > 0 {
        return Ok(response);
    }

    let mut insist_attempts = 0;
    while formula_count == 0 {
        insist_attempts += 1;
        if insist_attempts > MAX_INSIST_ATTEMPTS {
            anyhow::bail!("Failed to extract any SMT formula after {MAX_INSIST_ATTEMPTS} attempts");
        }
        info!(insist_attempts, "insisting on at least one formula");

        let role = if state.iteration == 0 {
            LlmRole::Formalizer
        } else {
            LlmRole::Fixer
        };

        let messages = vec![
            crate::llm::system_message(
                "You MUST output at least ONE SMT-LIB2 formula. \
                 Put each formula in a separate ```smt2 code block. \
                 Each formula must be complete with set-logic, declarations, assertions, and check-sat.",
            ),
            crate::llm::user_message(&format!(
                "Your previous response did not contain any valid SMT formula. \
                 Here was your previous response:\n\n{response}\n\n\
                 Please try again. Output at least ONE valid SMT-LIB2 formula in a ```smt2 code block.",
            )),
        ];

        response = llm.chat(role, messages).await?;
        formula_count = extract_all_formulas(&response).len();
    }

    Ok(response)
}

async fn call_generation(state: &WaitForGeneration, llm: &dyn LlmProvider) -> Result<String> {
    let role = if state.iteration == 0 {
        LlmRole::Formalizer
    } else {
        LlmRole::Fixer
    };

    let ctx = if state.insist_pending {
        crate::agent::build_generation_messages(&state.input_content, &GenerationContext::Global {
            verified: &state.verified,
            open: &state.open,
        })
    } else {
        build_generation_messages(&state.input_content, &GenerationContext::Global {
            verified: &state.verified,
            open: &state.open,
        })
    };

    llm.chat(role, ctx).await
}
