use anyhow::Result;
use std::sync::Arc;

use crate::provider::{AgentResult, LlmProvider, SolverProvider};
use crate::states::*;

pub async fn run(
    input_content: &str,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<AgentResult> {
    let input_arc = Arc::new(input_content.to_string());
    let mut state: Box<dyn AlgorithmState> = Box::new(WaitForSplit {
        input_content: Arc::clone(&input_arc),
        pieces_to_resplit: Vec::new(),
        verified: Vec::new(),
        open: Vec::new(),
        iteration: 0,
        split_depth: 0,
    });

    loop {
        state = match state.execute(llm, solver).await? {
            Step::State(next) => next,
            Step::Result(result) => return Ok(result),
        };
    }
}