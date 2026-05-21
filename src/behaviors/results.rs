use std::sync::Arc;

use anyhow::Result;
use futures::future;
use tracing::debug;

use crate::provider::{LlmProvider, LlmRole, SolverProvider};
use crate::smt::extract_all_formulas;
use crate::states::*;
use crate::transitions;

use super::child_judge;
use super::generation;

pub async fn execute_all(
    branches: Vec<FormulaBranch>,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<Vec<ChildDone>> {
    let futures = branches
        .into_iter()
        .map(|branch| run_branch(branch, llm, solver));
    let results: Vec<Vec<ChildDone>> = future::try_join_all(futures).await?;
    Ok(results.into_iter().flatten().collect())
}

async fn run_branch(
    branch: FormulaBranch,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<Vec<ChildDone>> {
    let piece = branch.piece;
    let mut retry_count = branch.retry_count;
    let mut phase = branch.phase.clone();
    let mut current_formula = branch.formula.clone();
    let input_content = branch.input_content;

    loop {
        match phase {
            BranchPhase::WaitForSolver { formula } => {
                debug!(piece_id = piece.id(), label = %piece.label(), "running solver");
                let result = solver.run(&formula, Some(&piece)).await?;
                debug!(piece_id = piece.id(), label = %piece.label(), outcome = ?result.outcome, "solver done");
                match transitions::transition_solver(formula.clone(), result) {
                    BranchFromSolver::Judge(f, r) => {
                        phase = BranchPhase::WaitForJudge {
                            formula: f,
                            solver_result: r,
                        };
                    }
                    BranchFromSolver::Error(f, r) => {
                        debug!(piece_id = piece.id(), label = %piece.label(), "solver error");
                        return Ok(vec![ChildDone::Open(OpenItem {
                            piece: Arc::clone(&piece),
                            formula: f,
                            reason: format!("Solver error: {}", r.stdout),
                            solver_stdout: r.stdout,
                            solver_stderr: r.stderr,
                        })]);
                    }
                    BranchFromSolver::Resplit(f, r) => {
                        debug!(piece_id = piece.id(), label = %piece.label(), "solver timeout, requesting resplit");
                        return Ok(vec![ChildDone::NeedsResplit {
                            piece: Arc::clone(&piece),
                            formula: f,
                            reason: r.stdout,
                        }]);
                    }
                }
            }
            BranchPhase::WaitForJudge {
                formula,
                solver_result,
            } => {
                let verdict = child_judge::execute(
                    &piece, &formula, &solver_result, llm, &input_content,
                ).await?;
                current_formula = formula.clone();
                match transitions::transition_judge(
                    formula,
                    Arc::clone(&piece),
                    solver_result,
                    verdict,
                    retry_count,
                ) {
                    BranchFromJudge::Verified(verified) => {
                        return Ok(vec![ChildDone::Verified(verified)]);
                    }
                    BranchFromJudge::Retry {
                        formula: _,
                        feedback,
                        solver_stdout,
                        solver_stderr,
                    } => {
                        retry_count += 1;
                        phase = BranchPhase::NeedFormula {
                            feedback: Some(feedback),
                            solver_stdout,
                            solver_stderr,
                            insist_pending: false,
                            insist_attempt: 0,
                            last_response: None,
                        };
                    }
                    BranchFromJudge::Exhausted {
                        formula,
                        piece: exhausted_piece,
                        feedback,
                        solver_stdout,
                        solver_stderr,
                    } => {
                        return Ok(vec![ChildDone::Open(OpenItem {
                            piece: exhausted_piece,
                            formula,
                            reason: format!("Branch retry exhausted: {feedback}"),
                            solver_stdout,
                            solver_stderr,
                        })]);
                    }
                }
            }
            BranchPhase::NeedFormula {
                ref feedback,
                ref solver_stdout,
                ref solver_stderr,
                insist_pending,
                insist_attempt,
                last_response,
            } => {
                let fb = feedback.as_deref().unwrap_or("");
                let role = LlmRole::Fixer;

                let response = if insist_pending {
                    let prev = last_response.as_deref().unwrap_or("");
                    debug!(piece_id = piece.id(), label = %piece.label(), "insist fixer retry for piece");
                    llm.chat(
                        role,
                        generation::build_retry_insist_messages(
                            &piece, fb, prev, &input_content,
                        ),
                        Some(&piece),
                    )
                    .await?
                } else {
                    debug!(piece_id = piece.id(), label = %piece.label(), "fixer retry for piece");
                    llm.chat(
                        role,
                        generation::build_retry_messages(
                            &piece, &current_formula, fb,
                            solver_stdout, solver_stderr, &input_content,
                        ),
                        Some(&piece),
                    )
                    .await?
                };

                let formulas = extract_all_formulas(&response);
                match transitions::transition_need_formula(formulas, insist_attempt) {
                    BranchFromNeedFormula::Proceed(formula) => {
                        phase = BranchPhase::WaitForSolver { formula };
                    }
                    BranchFromNeedFormula::Insist => {
                        phase = BranchPhase::NeedFormula {
                            feedback: Some(fb.to_string()),
                            solver_stdout: solver_stdout.clone(),
                            solver_stderr: solver_stderr.clone(),
                            insist_pending: true,
                            insist_attempt: insist_attempt + 1,
                            last_response: Some(response),
                        };
                    }
                    BranchFromNeedFormula::Exhausted(reason) => {
                        debug!(piece_id = piece.id(), label = %piece.label(), "need formula exhausted");
                        return Ok(vec![ChildDone::Open(OpenItem {
                            piece: Arc::clone(&piece),
                            formula: current_formula,
                            reason,
                            solver_stdout: solver_stdout.clone(),
                            solver_stderr: solver_stderr.clone(),
                        })]);
                    }
                }
            }
        }
    }
}