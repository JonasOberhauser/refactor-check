use anyhow::Result;
use std::sync::Arc;

use crate::behaviors;
use crate::consts::{MAX_GLOBAL_CYCLES, MAX_INSIST_ATTEMPTS};
use crate::provider::{AgentResult, LlmProvider, SolverProvider};
use crate::states::*;
use crate::transitions::build_result;

pub async fn run(
    input_content: &str,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<AgentResult> {
    let input_arc = Arc::new(input_content.to_string());
    let mut state = AlgorithmState::Idle;

    loop {
        state = match state {
            AlgorithmState::Idle => AlgorithmState::WaitForSplit(WaitForSplit {
                input_content: Arc::clone(&input_arc),
                pieces_to_resplit: Vec::new(),
                verified: Vec::new(),
                open: Vec::new(),
                iteration: 0,
                split_depth: 0,
                pending_pieces: Vec::new(),
            }),
            AlgorithmState::WaitForSplit(s) => {
                let s_open = s.open.clone();
                let pieces = behaviors::splitter::execute(&s, llm).await;
                match pieces {
                    Ok(pcs) => match s.transition(pcs) {
                        TransitionFromSplit::Generate(next) => AlgorithmState::WaitForGeneration(next),
                        TransitionFromSplit::Insist(next) => AlgorithmState::WaitForSplit(next),
                        TransitionFromSplit::Exhausted(msg) => anyhow::bail!(msg),
                        TransitionFromSplit::Open(open_items, next) => {
                            let mut open = s_open;
                            open.extend(open_items);
                            AlgorithmState::WaitForGeneration(WaitForGeneration {
                                open,
                                ..next
                            })
                        }
                    },
                    Err(e) => return Err(e),
                }
            }
            AlgorithmState::WaitForGeneration(s) => {
                if s.pieces.is_empty() {
                    if let InsistState::Insisting { attempt, .. } = &s.insist {
                        if *attempt >= MAX_INSIST_ATTEMPTS {
                            anyhow::bail!("failed to extract any SMT formula after {MAX_INSIST_ATTEMPTS} insist attempts");
                        }
                    }
                    let response = behaviors::generation::execute(&s, llm).await?;
                    match s.transition(response) {
                        TransitionFromGeneration::Results(next) => AlgorithmState::WaitForResults(next),
                        TransitionFromGeneration::Insist(next) => AlgorithmState::WaitForGeneration(next),
                    }
                } else {
                    let response = behaviors::generation::execute(&s, llm).await?;
                    match s.transition(response) {
                        TransitionFromGeneration::Results(next) => AlgorithmState::WaitForResults(next),
                        TransitionFromGeneration::Insist(next) => AlgorithmState::WaitForGeneration(next),
                    }
                }
            }
            AlgorithmState::WaitForResults(s) => {
                let done = behaviors::results::execute_all(&s, llm, solver).await?;
                match s.transition(done) {
                    TransitionFromResults::Generation(next) => {
                        if next.iteration >= MAX_GLOBAL_CYCLES {
                            let counts = OutcomeCounts::from_verified(&next.verified);
                            return Ok(build_result(
                                &next.verified,
                                next.open.len(),
                                &counts,
                            ));
                        }
                        AlgorithmState::WaitForGeneration(next)
                    }
                    TransitionFromResults::Resplit(next) => AlgorithmState::WaitForSplit(next),
                    TransitionFromResults::Explain(next) => AlgorithmState::WaitForExplanation(next),
                    TransitionFromResults::Done(result) => return Ok(result),
                }
            }
            AlgorithmState::WaitForExplanation(s) => {
                let explanations = behaviors::explain::execute(&s, llm).await?;
                return Ok(s.transition(explanations));
            }
        }
    }
}