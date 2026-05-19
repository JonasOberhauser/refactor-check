use anyhow::Result;
use futures::future;
use tracing::{info, warn};

use crate::agent::build_explanation_messages;
use crate::provider::{LlmProvider, LlmRole};
use crate::smt::SolverOutcome;
use crate::states::WaitForExplanation;

pub async fn execute(
    state: &WaitForExplanation,
    llm: &dyn LlmProvider,
) -> Result<Vec<Option<String>>> {
    let needs_explanation: Vec<(usize, String, SolverOutcome)> = state
        .result
        .formulas
        .iter()
        .enumerate()
        .filter(|(_, f)| matches!(f.outcome, SolverOutcome::Sat | SolverOutcome::Unknown))
        .map(|(i, f)| (i, f.formula.clone(), f.outcome.clone()))
        .collect();

    if needs_explanation.is_empty() {
        return Ok(Vec::new());
    }

    info!(count = needs_explanation.len(), "generating bug explanations");

    let futures = needs_explanation.iter().map(|(i, formula, outcome)| {
        let messages = build_explanation_messages(&state.input_content, formula, outcome);
        async move {
            let response = llm.chat(LlmRole::Fixer, messages).await;
            (*i, response)
        }
    });

    let results = future::join_all(futures).await;

    let total = state.result.formulas.len();
    let mut explanations = vec![None; total];

    for (i, response) in results {
        match response {
            Ok(text) => {
                info!("explanation received for formula {}", i);
                explanations[i] = Some(text);
            }
            Err(e) => {
                warn!(error = %e, "failed to get explanation for formula {}", i);
                explanations[i] = None;
            }
        }
    }

    Ok(explanations)
}
