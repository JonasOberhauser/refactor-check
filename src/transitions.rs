use std::sync::Arc;

use crate::consts::{JUDGE_REASONABLE, MAX_BRANCH_RETRIES, MAX_INSIST_ATTEMPTS, MAX_SPLIT_DEPTH};
use crate::provider::{AgentResult, FormulaResult};
use crate::smt::{SolverOutcome, SolverResult, extract_all_formulas};
use crate::states::*;

// ===== Global transitions =====

impl WaitForSplit {
    #[must_use]
    pub fn transition(
        self,
        pieces: Vec<CodePiece>,
    ) -> TransitionFromSplit {
        let mut all_pieces = self.pending_pieces;
        all_pieces.extend(pieces);

        let input_content = self.input_content;
        let verified = self.verified;
        let open = self.open;
        let iteration = self.iteration;

        if all_pieces.is_empty() {
            return TransitionFromSplit::Open(
                open.clone(),
                WaitForGeneration {
                    input_content,
                    verified,
                    open,
                    iteration,
                    insist: InsistState::Idle,
                    pieces: Vec::new(),
                },
            );
        }

        TransitionFromSplit::Generate(WaitForGeneration {
            input_content,
            verified,
            open,
            iteration,
            insist: InsistState::Idle,
            pieces: all_pieces,
        })
    }
}

impl WaitForGeneration {
    #[must_use]
    pub fn transition(self, llm_response: String) -> TransitionFromGeneration {
        let formulas = extract_all_formulas(&llm_response);
        let input_content = self.input_content;
        let verified = self.verified;
        let open = self.open;
        let iteration = self.iteration;
        let insist = self.insist;
        let pieces = self.pieces;

        if formulas.is_empty() {
            return TransitionFromGeneration::Insist(WaitForGeneration {
                input_content,
                verified,
                open,
                iteration,
                insist: InsistState::Insisting {
                    last_response: llm_response,
                    attempt: insist.attempt() + 1,
                },
                pieces,
            });
        }

        let branches: Vec<FormulaBranch> = if pieces.is_empty() {
            formulas
                .into_iter()
                .map(|formula| FormulaBranch {
                    piece: CodePiece {
                        label: String::new(),
                        before: String::new(),
                        after: String::new(),
                    },
                    input_content: Arc::clone(&input_content),
                    verified: verified.clone(),
                    formula: formula.clone(),
                    phase: BranchPhase::WaitForSolver { formula },
                    retry_count: 0,
                })
                .collect()
        } else {
            pieces
                .clone()
                .into_iter()
                .zip(formulas)
                .map(|(piece, formula)| FormulaBranch {
                    piece: piece.clone(),
                    input_content: Arc::clone(&input_content),
                    verified: verified.clone(),
                    formula: formula.clone(),
                    phase: BranchPhase::WaitForSolver { formula },
                    retry_count: 0,
                })
                .collect()
        };

        TransitionFromGeneration::Results(WaitForResults {
            input_content,
            verified,
            open,
            iteration,
            branches,
            split_depth: 0,
            remaining_pieces: Vec::new(),
        })
    }
}

impl WaitForResults {
    #[must_use]
    pub fn transition(self, done: Vec<ChildDone>) -> TransitionFromResults {
        let mut verified = self.verified;
        let mut open = self.open;
        let mut needs_resplit: Vec<(CodePiece, String)> = Vec::new();

        for cd in done {
            match cd {
                ChildDone::Verified(piece) => verified.push(piece),
                ChildDone::Open(item) => open.push(item),
                ChildDone::NeedsResplit(piece, _, reason) => {
                    needs_resplit.push((piece, reason));
                }
            }
        }

        if !needs_resplit.is_empty() {
            let new_depth = self.split_depth + 1;
            if new_depth >= MAX_SPLIT_DEPTH {
                for (piece, reason) in needs_resplit {
                    open.push(OpenItem {
                        formula: String::new(),
                        piece_label: piece.label,
                        reason: format!("split depth exhausted ({MAX_SPLIT_DEPTH}): {reason}"),
                        solver_stdout: String::new(),
                        solver_stderr: String::new(),
                    });
                }
            } else {
                return TransitionFromResults::Resplit(WaitForSplit {
                    input_content: Arc::clone(&self.input_content),
                    pieces_to_resplit: needs_resplit,
                    verified,
                    open,
                    iteration: self.iteration,
                    split_depth: new_depth,
                    pending_pieces: self.remaining_pieces,
                });
            }
        }

        let counts = OutcomeCounts::from_verified(&verified);

        if open.is_empty() {
            if counts.needs_explanation() {
                return TransitionFromResults::Explain(WaitForExplanation {
                    input_content: self.input_content,
                    result: build_result(&verified, 0, &counts),
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
            pieces: Vec::new(),
        })
    }
}

impl WaitForExplanation {
    #[must_use]
    pub fn transition(mut self, explanations: Vec<Option<String>>) -> AgentResult {
        for (f, explanation) in self.result.formulas.iter_mut().zip(explanations) {
            f.explanation = explanation;
        }
        self.result
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
        _ => BranchFromSolver::Judge(formula, result),
    }
}

pub fn transition_judge(
    formula: String,
    piece_label: String,
    solver_result: SolverResult,
    verdict: JudgeVerdict,
    retry_count: usize,
) -> BranchFromJudge {
    match verdict {
        JudgeVerdict::Reasonable => BranchFromJudge::Verified(VerifiedPiece {
            formula,
            piece_label,
            outcome: solver_result.outcome,
        }),
        JudgeVerdict::Retry(feedback) => {
            if retry_count + 1 >= MAX_BRANCH_RETRIES {
                BranchFromJudge::Exhausted {
                    formula,
                    piece_label,
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
                piece_label: v.piece_label.clone(),
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