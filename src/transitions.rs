use std::sync::Arc;

use crate::consts::JUDGE_REASONABLE;
use crate::consts::MAX_INSIST_ATTEMPTS;
use crate::provider::{AgentResult, FormulaResult};
use crate::smt::{SolverOutcome, extract_all_formulas};
use crate::states::*;

// ===== WaitForGeneration =====

impl WaitForGeneration {
    #[must_use]
    pub fn transition(self, llm_response: String) -> TransitionFromGeneration {
        let formulas = extract_all_formulas(&llm_response);
        if formulas.is_empty() {
            if self.insist_attempt >= MAX_INSIST_ATTEMPTS {
                return TransitionFromGeneration::Exhausted;
            }
            return TransitionFromGeneration::Insist(WaitForGeneration {
                input_content: self.input_content,
                verified: self.verified,
                open: self.open,
                iteration: self.iteration,
                insist_pending: true,
                insist_attempt: self.insist_attempt + 1,
                config: self.config,
            });
        }

        let branches: Vec<FormulaBranch> = formulas
            .into_iter()
            .map(|formula| FormulaBranch {
                input_content: Arc::clone(&self.input_content),
                verified: self.verified.clone(),
                region: Region::Formula(formula.clone()),
                state: BranchState::WaitForSolver { formula },
                retry_count: 0,
                config: self.config.clone(),
            })
            .collect();

        TransitionFromGeneration::Results(WaitForResults {
            input_content: self.input_content,
            verified: self.verified,
            open: self.open,
            iteration: self.iteration,
            branches,
            config: self.config,
        })
    }
}

// ===== WaitForResults =====

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

        let overall = build_result(&verified, open.len());

        if open.is_empty() {
            if overall.reasonable_sat > 0 || overall.reasonable_unknown > 0 {
                return TransitionFromResults::Explain(WaitForExplanation {
                    input_content: self.input_content,
                    result: overall,
                    config: self.config,
                });
            }
            return TransitionFromResults::Done(overall);
        }

        TransitionFromResults::Generation(WaitForGeneration {
            input_content: self.input_content,
            verified,
            open,
            iteration: self.iteration + 1,
            insist_pending: false,
            insist_attempt: 0,
            config: self.config,
        })
    }
}

// ===== WaitForExplanation =====

impl WaitForExplanation {
    #[must_use]
    pub fn transition(mut self, explanations: Vec<Option<String>>) -> TransitionFromExplanation {
        for (i, explanation) in explanations.into_iter().enumerate() {
            if let Some(text) = explanation {
                if let Some(f) = self.result.formulas.get_mut(i) {
                    f.explanation = Some(text);
                }
            }
        }
        TransitionFromExplanation::Done(self.result)
    }
}

// ===== Helpers =====

fn build_result(verified: &[VerifiedPiece], open_count: usize) -> AgentResult {
    let reasonable_sat = verified
        .iter()
        .filter(|v| matches!(v.outcome, SolverOutcome::Sat))
        .count();
    let reasonable_unsat = verified
        .iter()
        .filter(|v| matches!(v.outcome, SolverOutcome::Unsat))
        .count();
    let reasonable_unknown = verified
        .iter()
        .filter(|v| matches!(v.outcome, SolverOutcome::Unknown))
        .count();
    let overall_equivalent = open_count == 0 && reasonable_sat == 0 && reasonable_unknown == 0;

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
        overall_equivalent,
        open_count,
        reasonable_sat,
        reasonable_unsat,
        reasonable_unknown,
    }
}
