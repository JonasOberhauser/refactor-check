use anyhow::Result;
use std::sync::Arc;

use crate::behaviors;
use crate::consts::MAX_GLOBAL_CYCLES;
use crate::provider::{AgentResult, LlmProvider, SolverProvider};
use crate::states::*;

pub async fn run(
    input_content: &str,
    config: AgentConfig,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<AgentResult> {
    let input_arc = Arc::new(input_content.to_string());

    let mut state = AlgorithmState::Idle(Idle { config });

    loop {
        state = match state {
            AlgorithmState::Idle(s) => AlgorithmState::WaitForGeneration(WaitForGeneration {
                input_content: Arc::clone(&input_arc),
                verified: Vec::new(),
                open: Vec::new(),
                iteration: 0,
                insist_pending: false,
                insist_attempt: 0,
                config: s.config,
            }),
            AlgorithmState::WaitForGeneration(s) => {
                let open_count_on_exhaust = s.open.len();
                let response = behaviors::generation::execute(&s, llm).await?;
                match s.transition(response) {
                    TransitionFromGeneration::Results(next) => AlgorithmState::WaitForResults(next),
                    TransitionFromGeneration::Insist(next) => AlgorithmState::WaitForGeneration(next),
                    TransitionFromGeneration::Exhausted => {
                        AlgorithmState::Done(AgentResult {
                            formulas: Vec::new(),
                            overall_equivalent: false,
                            open_count: open_count_on_exhaust + 1,
                            reasonable_sat: 0,
                            reasonable_unsat: 0,
                            reasonable_unknown: 0,
                        })
                    }
                }
            }
            AlgorithmState::WaitForResults(s) => {
                let done = behaviors::results::execute_all(&s, llm, solver).await?;
                match s.transition(done) {
                    TransitionFromResults::Generation(next) => {
                        if next.iteration >= MAX_GLOBAL_CYCLES {
                            AlgorithmState::Done(build_global_done(
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

fn build_global_done(
    verified: &[VerifiedPiece],
    open_count: usize,
) -> AgentResult {
    let reasonable_sat = verified
        .iter()
        .filter(|v| matches!(v.outcome, crate::smt::SolverOutcome::Sat))
        .count();
    let reasonable_unsat = verified
        .iter()
        .filter(|v| matches!(v.outcome, crate::smt::SolverOutcome::Unsat))
        .count();
    let reasonable_unknown = verified
        .iter()
        .filter(|v| matches!(v.outcome, crate::smt::SolverOutcome::Unknown))
        .count();
    let overall_equivalent = open_count == 0 && reasonable_sat == 0 && reasonable_unknown == 0;

    AgentResult {
        formulas: verified
            .iter()
            .map(|v| crate::provider::FormulaResult {
                formula: v.formula.clone(),
                outcome: v.outcome.clone(),
                verdict: crate::consts::JUDGE_REASONABLE.to_string(),
                explanation: None,
            })
            .collect(),
        overall_equivalent,
        open_count,
        reasonable_sat,
        reasonable_unsat,
        reasonable_unknown,
    }
}
