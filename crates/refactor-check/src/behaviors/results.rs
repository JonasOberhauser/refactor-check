use std::sync::Arc;

use anyhow::Result;
use futures::future;
use tracing::debug;

use crate::phase::PiecePhase;
use crate::piece_manager::PieceManager;
use crate::provider::{DynLlmProvider, DynSolverProvider, LlmRequest, LlmRole, SolverRequest};
use crate::smt::extract_all_formulas;
use crate::states::*;
use crate::transitions;

use super::child_judge;
use super::generation;

pub async fn execute_all(
    branches: Vec<FormulaBranch>,
    llm: &DynLlmProvider,
    solver: &DynSolverProvider,
    pm: &dyn PieceManager,
) -> Result<Vec<ChildDone>> {
    let futures = branches
        .into_iter()
        .map(|branch| run_branch(branch, llm, solver, pm));
    let results: Vec<Vec<ChildDone>> = future::try_join_all(futures).await?;
    Ok(results.into_iter().flatten().collect())
}

async fn run_branch(
    branch: FormulaBranch,
    llm: &DynLlmProvider,
    solver: &DynSolverProvider,
    pm: &dyn PieceManager,
) -> Result<Vec<ChildDone>> {
    let piece = branch.piece;
    let mut retry_count = branch.retry_count;
    let mut phase = branch.phase.clone();
    let mut current_formula = branch.formula.clone();
    let input_content = branch.input_content;

    loop {
        match phase {
            BranchPhase::WaitForSolver { formula } => {
                piece.with_ctx(|ctx| pm.expect_any_and_set(ctx, &[PiecePhase::Forming, PiecePhase::Fixing], PiecePhase::Solving));
                debug!(ctx = %piece.ctx_display(), label = %piece.label(), "running solver");
                let ctx = piece.take_context();
                let resp = solver.invoke(SolverRequest { formula: formula.clone(), context_id: ctx }).await?;
                piece.restore_context(resp.context_id);
                let result = resp.value;
                debug!(ctx = %piece.ctx_display(), label = %piece.label(), outcome = ?result.outcome, "solver done");
                match transitions::transition_solver(formula.clone(), result) {
                    BranchFromSolver::Judge(f, r) => {
                        piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Judging));
                        phase = BranchPhase::WaitForJudge {
                            formula: f,
                            solver_result: r,
                        };
                    }
                    BranchFromSolver::Error(f, r) => {
                        piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Open));
                        debug!(ctx = %piece.ctx_display(), label = %piece.label(), "solver error");
                        return Ok(vec![ChildDone::Open(OpenItem {
                            piece: Arc::clone(&piece),
                            formula: f,
                            reason: format!("Solver error: {}", r.stdout),
                            solver_stdout: r.stdout,
                            solver_stderr: r.stderr,
                        })]);
                    }
                    BranchFromSolver::Resplit(f, r) => {
                        piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Open));
                        debug!(ctx = %piece.ctx_display(), label = %piece.label(), "solver timeout, requesting resplit");
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
                        piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Judging), PiecePhase::Verified));
                        return Ok(vec![ChildDone::Verified(verified)]);
                    }
                    BranchFromJudge::Retry {
                        formula: _,
                        feedback,
                        solver_stdout,
                        solver_stderr,
                    } => {
                        piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Judging), PiecePhase::Fixing));
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

                let ctx = piece.take_context();
                let resp = if insist_pending {
                    let prev = last_response.as_deref().unwrap_or("");
                    debug!(ctx = %piece.ctx_display(), label = %piece.label(), "insist fixer retry for piece");
                    llm.invoke(LlmRequest {
                        role,
                        messages: generation::build_retry_insist_messages(
                            &piece, fb, prev, &input_content,
                        ),
                        context_id: ctx,
                    }).await?
                } else {
                    debug!(ctx = %piece.ctx_display(), label = %piece.label(), "fixer retry for piece");
                    llm.invoke(LlmRequest {
                        role,
                        messages: generation::build_retry_messages(
                            &piece, &current_formula, fb,
                            solver_stdout, solver_stderr, &input_content,
                        ),
                        context_id: ctx,
                    }).await?
                };
                piece.restore_context(resp.context_id);
                let response = resp.value;

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
                        debug!(ctx = %piece.ctx_display(), label = %piece.label(), "need formula exhausted");
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