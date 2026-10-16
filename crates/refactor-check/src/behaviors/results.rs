use std::sync::Arc;

use anyhow::Result;
use refactor_check_core::checkpoint::{Cancellation, Handle};
use tracing::debug;

use crate::phase::PiecePhase;
use crate::piece_manager::PieceManager;
use crate::provider::{DynLlmProvider, DynSolverProvider, LlmRequest, LlmRole, SolverRequest};
use crate::smt::extract_all_formulas;
use crate::states::*;
use crate::transitions;

use super::child_judge;
use super::generation;

/// Advances the branch to an outcome, one IO call per step with a safepoint
/// before each; progress and outcome stay in the branch, so a branch restored
/// with an outcome does no work.
pub fn run_branch(
    mut h: Handle<'_, FormulaBranch>,
    llm: &DynLlmProvider,
    solver: &DynSolverProvider,
    pm: &dyn PieceManager,
    cancellation: &Cancellation,
) -> Result<()> {
    while h.done.is_none() {
        h.safepoint();
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let done = step(&mut h, llm, solver, pm)?;
        h.done = done;
    }
    Ok(())
}

fn step(
    branch: &mut FormulaBranch,
    llm: &DynLlmProvider,
    solver: &DynSolverProvider,
    pm: &dyn PieceManager,
) -> Result<Option<ChildDone>> {
    let piece = Arc::clone(&branch.piece);
    match branch.phase.clone() {
        BranchPhase::WaitForSolver { formula } => {
            piece.with_ctx(|ctx| pm.expect_any_and_set(ctx, &[PiecePhase::Forming, PiecePhase::Fixing], PiecePhase::Solving));
            debug!(ctx = %piece.ctx_display(), label = %piece.label(), "running solver");
            let ctx = piece.take_context().map_err(anyhow::Error::msg)?;
            let resp = solver.invoke(SolverRequest { formula: formula.clone(), context_id: ctx })?;
            piece.restore_context(resp.context_id);
            let result = resp.value;
            debug!(ctx = %piece.ctx_display(), label = %piece.label(), outcome = ?result.outcome, "solver done");
            match transitions::transition_solver(formula, result) {
                BranchFromSolver::Judge(f, r) => {
                    piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Judging));
                    branch.phase = BranchPhase::WaitForJudge {
                        formula: f,
                        solver_result: r,
                    };
                    Ok(None)
                }
                BranchFromSolver::Error(f, r) => {
                    piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Open));
                    debug!(ctx = %piece.ctx_display(), label = %piece.label(), "solver error");
                    Ok(Some(ChildDone::Open(OpenItem {
                        piece,
                        formula: f,
                        reason: format!("Solver error: {}", r.stdout),
                        solver_stdout: r.stdout,
                        solver_stderr: r.stderr,
                    })))
                }
                BranchFromSolver::Resplit(f, r) => {
                    piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Solving), PiecePhase::Open));
                    debug!(ctx = %piece.ctx_display(), label = %piece.label(), "solver timeout, requesting resplit");
                    Ok(Some(ChildDone::NeedsResplit {
                        piece,
                        formula: f,
                        reason: r.stdout,
                    }))
                }
            }
        }
        BranchPhase::WaitForJudge {
            formula,
            solver_result,
        } => {
            let verdict = child_judge::execute(
                &piece, &formula, &solver_result, llm, &branch.input_content,
            )?;
            branch.formula = formula.clone();
            match transitions::transition_judge(
                formula,
                Arc::clone(&piece),
                solver_result,
                verdict,
                branch.retry_count,
            ) {
                BranchFromJudge::Verified(verified) => {
                    piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Judging), PiecePhase::Verified));
                    Ok(Some(ChildDone::Verified(verified)))
                }
                BranchFromJudge::Retry {
                    formula: _,
                    feedback,
                    solver_stdout,
                    solver_stderr,
                } => {
                    piece.with_ctx(|ctx| pm.advance(ctx, Some(PiecePhase::Judging), PiecePhase::Fixing));
                    branch.retry_count += 1;
                    branch.phase = BranchPhase::NeedFormula {
                        feedback: Some(feedback),
                        solver_stdout,
                        solver_stderr,
                        insist_pending: false,
                        insist_attempt: 0,
                        last_response: None,
                    };
                    Ok(None)
                }
                BranchFromJudge::Exhausted {
                    formula,
                    piece: exhausted_piece,
                    feedback,
                    solver_stdout,
                    solver_stderr,
                } => Ok(Some(ChildDone::Open(OpenItem {
                    piece: exhausted_piece,
                    formula,
                    reason: format!("Branch retry exhausted: {feedback}"),
                    solver_stdout,
                    solver_stderr,
                }))),
            }
        }
        BranchPhase::NeedFormula {
            feedback,
            solver_stdout,
            solver_stderr,
            insist_pending,
            insist_attempt,
            last_response,
        } => {
            let fb = feedback.as_deref().unwrap_or("");
            let role = LlmRole::Fixer;

            let ctx = piece.take_context().map_err(anyhow::Error::msg)?;
            let resp = if insist_pending {
                let prev = last_response.as_deref().unwrap_or("");
                debug!(ctx = %piece.ctx_display(), label = %piece.label(), "insist fixer retry for piece");
                llm.invoke(LlmRequest {
                    role,
                    messages: generation::build_retry_insist_messages(
                        &piece, fb, prev, &branch.input_content,
                    ),
                    context_id: ctx,
                })?
            } else {
                debug!(ctx = %piece.ctx_display(), label = %piece.label(), "fixer retry for piece");
                llm.invoke(LlmRequest {
                    role,
                    messages: generation::build_retry_messages(
                        &piece, &branch.formula, fb,
                        &solver_stdout, &solver_stderr, &branch.input_content,
                    ),
                    context_id: ctx,
                })?
            };
            piece.restore_context(resp.context_id);
            let response = resp.value;

            let formulas = extract_all_formulas(&response);
            match transitions::transition_need_formula(formulas, insist_attempt) {
                BranchFromNeedFormula::Proceed(formula) => {
                    branch.phase = BranchPhase::WaitForSolver { formula };
                    Ok(None)
                }
                BranchFromNeedFormula::Insist => {
                    branch.phase = BranchPhase::NeedFormula {
                        feedback: Some(fb.to_string()),
                        solver_stdout,
                        solver_stderr,
                        insist_pending: true,
                        insist_attempt: insist_attempt + 1,
                        last_response: Some(response),
                    };
                    Ok(None)
                }
                BranchFromNeedFormula::Exhausted(reason) => {
                    debug!(ctx = %piece.ctx_display(), label = %piece.label(), "need formula exhausted");
                    Ok(Some(ChildDone::Open(OpenItem {
                        piece,
                        formula: branch.formula.clone(),
                        reason,
                        solver_stdout,
                        solver_stderr,
                    })))
                }
            }
        }
    }
}
