use std::sync::Arc;

use refactor_check_core::context_id::ContextId;
use tracing::debug;
use tracing::warn;

use crate::behaviors::{self, generation};
use crate::consts::{JUDGE_REASONABLE, MAX_BRANCH_RETRIES, MAX_GLOBAL_CYCLES, MAX_INSIST_ATTEMPTS, MAX_SPLIT_DEPTH};
use crate::piece_manager::PieceManager;
use crate::result::{AgentResult, FormulaResult};
use crate::provider::{DynLlmProvider, DynSolverProvider};
use crate::smt::{SolverOutcome, SolverResult};
use crate::states::*;

// ===== State machine =====

impl AlgorithmState for WaitForSplit {
    fn execute(
        self,
        llm: &DynLlmProvider,
        _solver: &DynSolverProvider,
        pm: &dyn PieceManager,
    ) -> anyhow::Result<Step> {
        let raw_pieces = behaviors::splitter::execute(&self, llm, pm, ContextId::root())?;
        let pieces: Vec<Arc<CodePiece>> = raw_pieces.into_iter().map(Arc::new).collect();
        Ok(Step::State(Stage::SplittingJudge(WaitForSplittingJudge {
            input_content: self.input_content,
            pieces,
            pieces_to_resplit: self.pieces_to_resplit,
            verified: self.verified,
            open: self.open,
            iteration: self.iteration,
            split_depth: self.split_depth,
        })))
    }
}

impl AlgorithmState for WaitForSplittingJudge {
    fn execute(
        self,
        llm: &DynLlmProvider,
        _solver: &DynSolverProvider,
        _pm: &dyn PieceManager,
    ) -> anyhow::Result<Step> {
        let (verdict, _) = behaviors::splitting_judge::execute(&self, llm, ContextId::root().new_child())?;
        match verdict {
            behaviors::splitting_judge::SplittingJudgeVerdict::Accept => {
                debug!("splitting judge accepted the decomposition");
                Ok(Step::State(Stage::Generation(WaitForGeneration {
                    input_content: self.input_content,
                    verified: self.verified,
                    open: self.open,
                    iteration: self.iteration,
                    insist: InsistState::Idle,
                    pieces: self.pieces.into_iter().map(GenerationSlot::new).collect(),
                })))
            }
            behaviors::splitting_judge::SplittingJudgeVerdict::Retry(feedback) => {
                let new_depth = self.split_depth + 1;
                if new_depth >= MAX_SPLIT_DEPTH {
                    warn!(depth = new_depth, "split depth exhausted, pieces become open");
                    let mut open = self.open;
                    for piece in self.pieces {
                        open.push(OpenItem {
                            piece,
                            formula: String::new(),
                            reason: format!("split depth exhausted ({MAX_SPLIT_DEPTH}): {feedback}"),
                            solver_stdout: String::new(),
                            solver_stderr: String::new(),
                        });
                    }
                    let counts = OutcomeCounts::from_verified(&self.verified);
                    if open.is_empty() {
                        if counts.needs_explanation() {
                            return Ok(Step::State(Stage::Explanation(WaitForExplanation::new(
                                self.input_content,
                                build_result(&self.verified, 0, &counts),
                            ))));
                        }
                        return Ok(Step::Result(build_result(&self.verified, 0, &counts)));
                    }
                    let iteration = self.iteration;
                    if iteration >= MAX_GLOBAL_CYCLES {
                        let counts = OutcomeCounts::from_verified(&self.verified);
                        return Ok(Step::Result(build_result(
                            &self.verified,
                            open.len(),
                            &counts,
                        )));
                    }
                    let pieces: Vec<GenerationSlot> =
                        open.iter().map(|o| GenerationSlot::new(Arc::clone(&o.piece))).collect();
                    return Ok(Step::State(Stage::Generation(WaitForGeneration {
                        input_content: self.input_content,
                        verified: self.verified,
                        open: Vec::new(),
                        iteration,
                        insist: InsistState::Idle,
                        pieces,
                    })));
                }
                debug!(depth = new_depth, feedback = %feedback, "splitting judge rejected, re-splitting");
                Ok(Step::State(Stage::Split(WaitForSplit {
                    input_content: self.input_content,
                    pieces_to_resplit: self.pieces_to_resplit,
                    verified: self.verified,
                    open: self.open,
                    iteration: self.iteration,
                    split_depth: new_depth,
                    judge_feedback: Some(feedback),
                })))
            }
        }
    }
}

impl AlgorithmState for WaitForGeneration {
    fn execute(
        self,
        llm: &DynLlmProvider,
        _solver: &DynSolverProvider,
        pm: &dyn PieceManager,
    ) -> anyhow::Result<Step> {
        let formulas = match &self.insist {
            InsistState::Insisting { attempt, last_response } => {
                if *attempt >= MAX_INSIST_ATTEMPTS {
                    anyhow::bail!("failed to extract any SMT formula after {MAX_INSIST_ATTEMPTS} insist attempts");
                }
                generation::generate_insist(&self, llm, pm, last_response)?
            }
            InsistState::Idle => generation::collect(&self),
        };
        let pieces = self.pieces.into_iter().map(|slot| slot.piece);
        if formulas.is_empty() {
            return Ok(Step::State(Stage::Generation(WaitForGeneration {
                input_content: self.input_content,
                verified: self.verified,
                open: self.open,
                iteration: self.iteration,
                insist: InsistState::Insisting {
                    last_response: String::new(),
                    attempt: self.insist.attempt() + 1,
                },
                pieces: pieces.map(GenerationSlot::new).collect(),
            })));
        }

        let branches: Vec<FormulaBranch> = pieces
            .zip(formulas)
            .map(|(piece, formula)| {
                debug!(ctx = %piece.ctx_display(), label = %piece.label(), "paired piece with formula");
                FormulaBranch {
                    piece,
                    input_content: Arc::clone(&self.input_content),
                    formula: formula.clone(),
                    phase: BranchPhase::WaitForSolver {
                        formula,
                    },
                    retry_count: 0,
                    done: None,
                }
            })
            .collect();

        Ok(Step::State(Stage::Results(WaitForResults {
            input_content: self.input_content,
            verified: self.verified,
            open: self.open,
            iteration: self.iteration,
            branches,
        })))
    }
}

impl AlgorithmState for WaitForResults {
    fn execute(
        self,
        _llm: &DynLlmProvider,
        _solver: &DynSolverProvider,
        _pm: &dyn PieceManager,
    ) -> anyhow::Result<Step> {
        let WaitForResults { input_content, verified, open, iteration, branches } = self;

        let mut verified = verified;
        let mut open = open;
        let mut needs_resplit: Vec<(Arc<CodePiece>, String)> = Vec::new();

        for branch in branches {
            let done = branch.done.ok_or_else(|| {
                anyhow::anyhow!("branch {} ended without an outcome", branch.piece.label())
            })?;
            match done {
                ChildDone::Verified(piece) => verified.push(piece),
                ChildDone::Open(item) => open.push(item),
                ChildDone::NeedsResplit { piece, formula: _, reason } => {
                    needs_resplit.push((piece, reason));
                }
            }
        }

        if !needs_resplit.is_empty() {
            return Ok(Step::State(Stage::Split(WaitForSplit {
                input_content: Arc::clone(&input_content),
                pieces_to_resplit: needs_resplit,
                verified,
                open,
                iteration,
                split_depth: 0,
                judge_feedback: None,
            })));
        }

        let counts = OutcomeCounts::from_verified(&verified);

        if open.is_empty() {
            if counts.needs_explanation() {
                return Ok(Step::State(Stage::Explanation(WaitForExplanation::new(
                    input_content,
                    build_result(&verified, 0, &counts),
                ))));
            }
            return Ok(Step::Result(build_result(&verified, 0, &counts)));
        }

        let next_iteration = iteration + 1;
        let pieces: Vec<GenerationSlot> =
            open.iter().map(|o| GenerationSlot::new(Arc::clone(&o.piece))).collect();
        let next = WaitForGeneration {
            input_content,
            verified,
            open: Vec::new(),
            iteration: next_iteration,
            insist: InsistState::Idle,
            pieces,
        };

        if next_iteration >= MAX_GLOBAL_CYCLES {
            let counts = OutcomeCounts::from_verified(&next.verified);
            return Ok(Step::Result(build_result(
                &next.verified,
                next.open.len(),
                &counts,
            )));
        }

        Ok(Step::State(Stage::Generation(next)))
    }
}

impl AlgorithmState for WaitForExplanation {
    fn execute(
        self,
        _llm: &DynLlmProvider,
        _solver: &DynSolverProvider,
        _pm: &dyn PieceManager,
    ) -> anyhow::Result<Step> {
        let mut result = self.result;
        for slot in self.explanations {
            if let Some(f) = result.formulas.get_mut(slot.formula_index) {
                f.explanation = slot.explanation;
            }
        }
        Ok(Step::Result(result))
    }
}

impl AlgorithmState for Stage {
    fn execute(
        self,
        llm: &DynLlmProvider,
        solver: &DynSolverProvider,
        pm: &dyn PieceManager,
    ) -> anyhow::Result<Step> {
        match self {
            Stage::Split(s) => s.execute(llm, solver, pm),
            Stage::SplittingJudge(s) => s.execute(llm, solver, pm),
            Stage::Generation(s) => s.execute(llm, solver, pm),
            Stage::Results(s) => s.execute(llm, solver, pm),
            Stage::Explanation(s) => s.execute(llm, solver, pm),
        }
    }
}

// ===== Branch transitions =====

pub fn transition_need_formula(
    formulas: Vec<String>,
    insist_attempt: usize,
) -> BranchFromNeedFormula {
    match formulas.into_iter().next() {
        Some(first) => BranchFromNeedFormula::Proceed(first),
        None if insist_attempt >= MAX_INSIST_ATTEMPTS => BranchFromNeedFormula::Exhausted(format!(
            "failed to produce formula after {MAX_INSIST_ATTEMPTS} insist attempts",
        )),
        None => BranchFromNeedFormula::Insist,
    }
}

pub fn transition_solver(formula: String, result: SolverResult) -> BranchFromSolver {
    match result.outcome {
        SolverOutcome::Error(_) => BranchFromSolver::Error(formula, result),
        SolverOutcome::Unknown => BranchFromSolver::Resplit(formula, result),
        SolverOutcome::Sat | SolverOutcome::Unsat => BranchFromSolver::Judge(formula, result),
    }
}

pub fn transition_judge(
    formula: String,
    piece: Arc<CodePiece>,
    solver_result: SolverResult,
    verdict: JudgeVerdict,
    retry_count: usize,
) -> BranchFromJudge {
    match verdict {
        JudgeVerdict::Reasonable => BranchFromJudge::Verified(VerifiedPiece {
            piece,
            formula,
            outcome: solver_result.outcome,
        }),
        JudgeVerdict::Retry(feedback) => {
            if retry_count + 1 >= MAX_BRANCH_RETRIES {
                BranchFromJudge::Exhausted {
                    formula,
                    piece,
                    feedback,
                    solver_stdout: solver_result.stdout,
                    solver_stderr: solver_result.stderr,
                }
            } else {
                BranchFromJudge::Retry {
                    formula,
                    feedback,
                    solver_stdout: solver_result.stdout,
                    solver_stderr: solver_result.stderr,
                }
            }
        }
    }
}

pub fn build_result(
    verified: &[VerifiedPiece],
    open_count: usize,
    counts: &OutcomeCounts,
) -> AgentResult {
    AgentResult {
        formulas: verified
            .iter()
            .map(|v| FormulaResult {
                formula: v.formula.clone(),
                piece_label: v.piece.label().to_string(),
                outcome: v.outcome.clone(),
                verdict: JUDGE_REASONABLE.to_string(),
                explanation: None,
            })
            .collect(),
        overall_equivalent: counts.overall_equivalent(open_count),
        open_count,
        reasonable_sat: counts.sat,
        reasonable_unsat: counts.unsat,
        reasonable_unknown: counts.unknown,
    }
}