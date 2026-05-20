use std::sync::Arc;

use async_trait::async_trait;
use tracing::debug;

use crate::behaviors::{self, generation};
use crate::consts::{JUDGE_REASONABLE, MAX_BRANCH_RETRIES, MAX_GLOBAL_CYCLES, MAX_INSIST_ATTEMPTS, MAX_SPLIT_DEPTH};
use crate::provider::{AgentResult, FormulaResult, LlmProvider, SolverProvider};
use crate::smt::{SolverOutcome, SolverResult};
use crate::states::*;

// ===== State machine =====

#[async_trait]
impl AlgorithmState for WaitForSplit {
    async fn execute(
        self: Box<Self>,
        llm: &dyn LlmProvider,
        _solver: &dyn SolverProvider,
    ) -> anyhow::Result<Step> {
        let pieces = behaviors::splitter::execute(&self, llm).await?;
        Ok(Step::State(Box::new(WaitForGeneration {
            input_content: self.input_content,
            verified: self.verified,
            open: self.open,
            iteration: self.iteration,
            insist: InsistState::Idle,
            pieces,
        })))
    }
}

#[async_trait]
impl AlgorithmState for WaitForGeneration {
    async fn execute(
        self: Box<Self>,
        llm: &dyn LlmProvider,
        _solver: &dyn SolverProvider,
    ) -> anyhow::Result<Step> {
        if let InsistState::Insisting { attempt, .. } = &self.insist {
            if *attempt >= MAX_INSIST_ATTEMPTS {
                anyhow::bail!("failed to extract any SMT formula after {MAX_INSIST_ATTEMPTS} insist attempts");
            }
        }
        let formula_pairs = generation::execute(&self, llm).await?;
        if formula_pairs.is_empty() {
            return Ok(Step::State(Box::new(WaitForGeneration {
                input_content: self.input_content,
                verified: self.verified,
                open: self.open,
                iteration: self.iteration,
                insist: InsistState::Insisting {
                    last_response: String::new(),
                    attempt: self.insist.attempt() + 1,
                },
                pieces: self.pieces,
            })));
        }

        let branches: Vec<FormulaBranch> = formula_pairs
            .into_iter()
            .map(|pf| {
                assert!(
                    pf.piece.id != 0,
                    "piece {} #{} has invalid id",
                    pf.piece.label,
                    pf.piece.id,
                );
                debug!(piece_id = pf.piece.id, label = %pf.piece.label, "paired piece with formula");
                FormulaBranch {
                    piece: pf.piece,
                    input_content: Arc::clone(&self.input_content),
                    verified: self.verified.clone(),
                    formula: pf.formula.clone(),
                    phase: BranchPhase::WaitForSolver {
                        formula: pf.formula,
                    },
                    retry_count: 0,
                }
            })
            .collect();

        Ok(Step::State(Box::new(WaitForResults {
            input_content: self.input_content,
            verified: self.verified,
            open: self.open,
            iteration: self.iteration,
            branches,
            split_depth: 0,
        })))
    }
}

#[async_trait]
impl AlgorithmState for WaitForResults {
    async fn execute(
        self: Box<Self>,
        llm: &dyn LlmProvider,
        solver: &dyn SolverProvider,
    ) -> anyhow::Result<Step> {
        let done = behaviors::results::execute_all(&self, llm, solver).await?;

        let mut verified = self.verified;
        let mut open = self.open;
        let mut needs_resplit: Vec<(CodePiece, String)> = Vec::new();

        for cd in done {
            match cd {
                ChildDone::Verified(piece) => verified.push(piece),
                ChildDone::Open(item) => open.push(item),
                ChildDone::NeedsResplit { piece, formula: _, reason } => {
                    needs_resplit.push((piece, reason));
                }
            }
        }

        if !needs_resplit.is_empty() {
            let new_depth = self.split_depth + 1;
            if new_depth >= MAX_SPLIT_DEPTH {
                for (piece, reason) in needs_resplit {
                    open.push(OpenItem {
                        piece,
                        formula: String::new(),
                        reason: format!("split depth exhausted ({MAX_SPLIT_DEPTH}): {reason}"),
                        solver_stdout: String::new(),
                        solver_stderr: String::new(),
                    });
                }
            } else {
                return Ok(Step::State(Box::new(WaitForSplit {
                    input_content: Arc::clone(&self.input_content),
                    pieces_to_resplit: needs_resplit,
                    verified,
                    open,
                    iteration: self.iteration,
                    split_depth: new_depth,
                })));
            }
        }

        let counts = OutcomeCounts::from_verified(&verified);

        if open.is_empty() {
            if counts.needs_explanation() {
                return Ok(Step::State(Box::new(WaitForExplanation {
                    input_content: self.input_content,
                    result: build_result(&verified, 0, &counts),
                })));
            }
            return Ok(Step::Result(build_result(&verified, 0, &counts)));
        }

        let next_iteration = self.iteration + 1;
        let pieces: Vec<CodePiece> = open.iter().map(|o| o.piece.clone()).collect();
        let next = WaitForGeneration {
            input_content: self.input_content,
            verified,
            open,
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

        Ok(Step::State(Box::new(next)))
    }
}

#[async_trait]
impl AlgorithmState for WaitForExplanation {
    async fn execute(
        self: Box<Self>,
        llm: &dyn LlmProvider,
        _solver: &dyn SolverProvider,
    ) -> anyhow::Result<Step> {
        let explanations = behaviors::explain::execute(&self, llm).await?;
        let mut result = self.result;
        for (f, explanation) in result.formulas.iter_mut().zip(explanations) {
            f.explanation = explanation;
        }
        Ok(Step::Result(result))
    }
}

// ===== Branch transitions =====

pub fn transition_need_formula(
    formulas: Vec<String>,
    insist_attempt: usize,
) -> BranchFromNeedFormula {
    if formulas.is_empty() {
        if insist_attempt >= MAX_INSIST_ATTEMPTS {
            return BranchFromNeedFormula::Exhausted(format!(
                "failed to produce formula after {MAX_INSIST_ATTEMPTS} insist attempts",
            ));
        }
        return BranchFromNeedFormula::Insist;
    }
    BranchFromNeedFormula::Proceed(formulas.into_iter().next().unwrap())
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
    piece: CodePiece,
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
                piece_id: v.piece.id,
                piece_label: v.piece.label.clone(),
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