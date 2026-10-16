use refactor_check_core::checkpoint::Handle;
use tracing::{info, warn};

use crate::agent::build_explanation_messages;
use crate::provider::{DynLlmProvider, LlmRequest, LlmRole};
use crate::states::ExplanationSlot;

/// Stores an explanation of the slot's formula, unless an earlier run already
/// did or failed to. A failed request leaves it `None`.
pub fn explain(mut slot: Handle<'_, ExplanationSlot>, input_content: &str, llm: &DynLlmProvider) {
    if slot.explanation.is_some() {
        return;
    }
    let Some(ctx) = slot.ctx.take() else {
        return;
    };
    let messages = build_explanation_messages(input_content, &slot.formula, &slot.outcome);
    match llm.invoke(LlmRequest { role: LlmRole::Fixer, messages, context_id: ctx }) {
        Ok(resp) => {
            info!(ctx = %resp.context_id, "explanation received for formula {}", slot.formula_index);
            slot.ctx = Some(resp.context_id);
            slot.explanation = Some(resp.value);
        }
        Err(e) => {
            warn!(error = %e, "failed to get explanation for formula {}", slot.formula_index);
        }
    }
}
