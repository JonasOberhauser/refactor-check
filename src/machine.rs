use anyhow::Result;
use std::sync::Arc;

use crate::behaviors;
use crate::consts::MAX_GLOBAL_CYCLES;
use crate::consts::MAX_INSIST_ATTEMPTS;
use crate::provider::{AgentResult, LlmProvider, SolverProvider};
use crate::states::*;
use crate::transitions::build_result;

pub async fn run(
    input_content: &str,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<AgentResult> {
    let input_arc = Arc::new(input_content.to_string());
    let mut state = AlgorithmState::Idle(Idle);

    loop {
        state = match state {
            AlgorithmState::Idle(_) => AlgorithmState::WaitForGeneration(WaitForGeneration {
                input_content: Arc::clone(&input_arc),
                verified: Vec::new(),
                open: Vec::new(),
                iteration: 0,
                insist_pending: false,
                insist_attempt: 0,
                last_response: None,
            }),
            AlgorithmState::WaitForGeneration(s) => {
                if s.insist_attempt > MAX_INSIST_ATTEMPTS {
                    anyhow::bail!("failed to extract any SMT formula after {MAX_INSIST_ATTEMPTS} insist attempts");
                }
                let response = behaviors::generation::execute(&s, llm).await?;
                match s.transition(response) {
                    TransitionFromGeneration::Results(next) => AlgorithmState::WaitForResults(next),
                    TransitionFromGeneration::Insist(next) => AlgorithmState::WaitForGeneration(next),
                }
            }
            AlgorithmState::WaitForResults(s) => {
                let done = behaviors::results::execute_all(&s, llm, solver).await?;
                match s.transition(done) {
                    TransitionFromResults::Generation(next) => {
                        if next.iteration >= MAX_GLOBAL_CYCLES {
                            AlgorithmState::Done(build_result(
                                &next.verified,
                                next.open.len(),
                            ))
                        } else {
                            AlgorithmState::WaitForGeneration(next)
                        }
                    }
                    TransitionFromResults::Explain(next) => AlgorithmState::WaitForExplanation(next),
                    TransitionFromResults::Done(result) => return Ok(result),
                }
            }
            AlgorithmState::WaitForExplanation(s) => {
                let explanations = behaviors::explain::execute(&s, llm).await?;
                match s.transition(explanations) {
                    TransitionFromExplanation::Done(result) => return Ok(result),
                }
            }
            AlgorithmState::Done(result) => return Ok(result),
        }
    }
}