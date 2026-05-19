use anyhow::Result;

use crate::provider::{LlmProvider, LlmRole};
use crate::states::WaitForGeneration;

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
) -> Result<String> {
    let role = role_for_iteration(state.iteration);

    if state.insist_pending {
        let prev = state
            .last_response
            .as_deref()
            .unwrap_or("(no previous response)");
        let messages = vec![
            crate::llm::system_message(
                "You MUST output at least ONE SMT-LIB2 formula. \
                 Put each formula in a separate ```smt2 code block. \
                 Each formula must be complete with set-logic, declarations, assertions, and check-sat.",
            ),
            crate::llm::user_message(&format!(
                "Your previous response did not contain any valid SMT formula. \
                 Here was your previous response:\n\n{prev}\n\n\
                 Please try again. Output at least ONE valid SMT-LIB2 formula in a ```smt2 code block.",
            )),
        ];
        return llm.chat(role, messages).await;
    }

    let messages = crate::agent::build_generation_messages(
        &state.input_content,
        &crate::agent::GenerationContext::Global {
            verified: &state.verified,
            open: &state.open,
        },
    );
    llm.chat(role, messages).await
}