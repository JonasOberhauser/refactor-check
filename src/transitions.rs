use std::sync::Arc;

use crate::consts::{JUDGE_REASONABLE, MAX_BRANCH_RETRIES, MAX_INSIST_ATTEMPTS};
use crate::provider::{AgentResult, FormulaResult};
use crate::smt::{SolverOutcome, SolverResult, extract_all_formulas};
use crate::states::*;

// ===== Global transitions =====

impl WaitForGeneration {
    #[must_use]
    pub fn transition(self, llm_response: String) -> TransitionFromGeneration {
        let formulas = extract_all_formulas(&llm_response);
        if formulas.is_empty() {
            return TransitionFromGeneration::Insist(WaitForGeneration {
                input_content: self.input_content,
                verified: self.verified,
                open: self.open,
                iteration: self.iteration,
                insist: InsistState::Insisting {
                    last_response: llm_response,
                },
                insist_attempt: self.insist_attempt + 1,
            });
        }

        let branches: Vec<FormulaBranch> = formulas
            .into_iter()
            .map(|formula| FormulaBranch {
                input_content: Arc::clone(&self.input_content),
                verified: self.verified.clone(),
                formula_id: formula.clone(),
                phase: BranchPhase::WaitForSolver { formula },
                retry_count: 0,
            })
            .collect();

        TransitionFromGeneration::Results(WaitForResults {
            input_content: self.input_content,
            verified: self.verified,
            open: self.open,
            iteration: self.iteration,
            branches,
        })
    }
}

impl WaitForResults {
    #[must_use]
    pub fn transition(self, done: Vec<ChildDone>) -> TransitionFromResults {
        let mut verified = self.verified;
        let mut open = self.open;

        for cd in done {
            match cd {
                ChildDone::Verified(piece) => verified.push(piece),
                ChildDone::Open(item) => open.push(item),
            }
        }

        let counts = OutcomeCounts::from_verified(&verified);

        if open.is_empty() {
            if counts.needs_explanation() {
                return TransitionFromResults::Explain(WaitForExplanation {
                    input_content: self.input_content,
                    result: build_result(&verified, open.len(), &counts),
                });
            }
            return TransitionFromResults::Done(build_result(&verified, 0, &counts));
        }

        TransitionFromResults::Generation(WaitForGeneration {
            input_content: self.input_content,
            verified,
            open,
            iteration: self.iteration + 1,
            insist: InsistState::Idle,
            insist_attempt: 0,
        })
    }
}

impl WaitForExplanation {
    #[must_use]
    pub fn transition(mut self, explanations: Vec<Option<String>>) -> TransitionFromExplanation {
        for (f, explanation) in self.result.formulas.iter_mut().zip(explanations) {
            f.explanation = explanation;
        }
        TransitionFromExplanation::Done(self.result)
    }
}

// ===== Branch transitions =====

pub fn transition_need_formula(formulas: Vec<String>, insist_attempt: usize) -> BranchFromNeedFormula {
    if formulas.is_empty() {
        if insist_attempt >= MAX_INSIST_ATTEMPTS {
            return BranchFromNeedFormula::Exhausted(
                format!("failed to produce formula after {MAX_INSIST_ATTEMPTS} insist attempts"),
            );
        }
        return BranchFromNeedFormula::Insist;
    }
    if formulas.len() == 1 {
        return BranchFromNeedFormula::Proceed(formulas.into_iter().next().unwrap());
    }
    BranchFromNeedFormula::FanOut(formulas)
}

pub fn transition_solver(formula: String, result: SolverResult) -> BranchFromSolver {
    if matches!(result.outcome, SolverOutcome::Error(_)) {
        BranchFromSolver::Error(formula, result)
    } else {
        BranchFromSolver::Judge(formula, result)
    }
}

pub fn transition_judge(
    formula: String,
    solver_result: SolverResult,
    verdict: JudgeVerdict,
    retry_count: usize,
) -> BranchFromJudge {
    match verdict {
        JudgeVerdict::Reasonable => BranchFromJudge::Verified(VerifiedPiece {
            formula,
            outcome: solver_result.outcome,
        }),
        JudgeVerdict::Retry(feedback) => {
            if retry_count + 1 >= MAX_BRANCH_RETRIES {
                BranchFromJudge::Exhausted {
                    formula,
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

// ===== Helpers =====

pub fn build_result(verified: &[VerifiedPiece], open_count: usize, counts: &OutcomeCounts) -> AgentResult {
    AgentResult {
        formulas: verified
            .iter()
            .map(|v| FormulaResult {
                formula: v.formula.clone(),
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