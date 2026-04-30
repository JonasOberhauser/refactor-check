use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

use crate::llm::Message;
use crate::smt::{SolverOutcome, SolverResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRole {
    Primary,
    Judge,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, role: LlmRole, messages: Vec<Message>) -> Result<String>;
}

#[async_trait]
pub trait SolverProvider: Send + Sync {
    async fn run(&self, formula: &str) -> Result<SolverResult>;
}

pub struct AgentResult {
    pub analysis: String,
    pub formula: String,
}

pub struct SequenceLlm {
    primary: Arc<Mutex<Vec<String>>>,
    judge: Arc<Mutex<Vec<String>>>,
}

impl SequenceLlm {
    pub fn new(primary: Vec<String>, judge: Vec<String>) -> Self {
        Self {
            primary: Arc::new(Mutex::new(primary)),
            judge: Arc::new(Mutex::new(judge)),
        }
    }

    pub fn primary_remaining(&self) -> usize {
        self.primary.lock().expect("lock poisoned").len()
    }

    pub fn judge_remaining(&self) -> usize {
        self.judge.lock().expect("lock poisoned").len()
    }
}

#[async_trait]
impl LlmProvider for SequenceLlm {
    async fn chat(&self, role: LlmRole, _messages: Vec<Message>) -> Result<String> {
        let queue = match role {
            LlmRole::Primary => &self.primary,
            LlmRole::Judge => &self.judge,
        };
        let mut responses = queue.lock().expect("lock poisoned");
        if responses.is_empty() {
            anyhow::bail!("SequenceLlm: no more {:?} responses available", role);
        }
        Ok(responses.remove(0))
    }
}

pub struct FakeSolver {
    pub outcome: SolverOutcome,
}

#[async_trait]
impl SolverProvider for FakeSolver {
    async fn run(&self, _formula: &str) -> Result<SolverResult> {
        Ok(SolverResult {
            outcome: self.outcome.clone(),
            stdout: match &self.outcome {
                SolverOutcome::Unsat => "unsat".to_string(),
                SolverOutcome::Sat => "sat".to_string(),
                SolverOutcome::Unknown => "unknown".to_string(),
                SolverOutcome::Error(e) => e.clone(),
            },
            stderr: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smt_formula() -> String {
        "\
(set-logic QF_LIA)
(declare-fun x () Int)
(declare-fun y () Int)
(assert (= x y))
(check-sat)"
            .to_string()
    }

    fn formula_response() -> String {
        format!("Here is the formula:\n\n```smt2\n{}\n```", smt_formula())
    }

    #[test_log::test(tokio::test)]
    async fn test_happy_path_unsat() {
        let llm = SequenceLlm::new(
            vec![formula_response(), "The formula is unsat, meaning equivalence.".to_string()],
            vec!["YES".to_string()],
        );
        let solver = FakeSolver { outcome: SolverOutcome::Unsat };

        let result = crate::agent::run_with_providers("refactoring desc", &llm, &solver)
            .await
            .expect("agent should succeed");

        assert!(!result.analysis.is_empty());
        assert!(result.formula.contains("(set-logic"));
        assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
        assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
    }

    #[test_log::test(tokio::test)]
    async fn test_formula_not_found_then_found() {
        let llm = SequenceLlm::new(
            vec![
                "I don't have a formula yet.".to_string(),
                formula_response(),
                "The solver says unsat, which is correct.".to_string(),
            ],
            vec!["YES".to_string()],
        );
        let solver = FakeSolver { outcome: SolverOutcome::Unsat };

        let result = crate::agent::run_with_providers("refactoring desc", &llm, &solver)
            .await
            .expect("agent should succeed");

        assert!(!result.analysis.is_empty());
        assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
        assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
    }

    #[test_log::test(tokio::test)]
    async fn test_solver_error_then_success() {
        struct ToggleSolver {
            call_count: Arc<Mutex<usize>>,
        }

        #[async_trait]
        impl SolverProvider for ToggleSolver {
            async fn run(&self, _formula: &str) -> Result<SolverResult> {
                let mut count = self.call_count.lock().expect("lock poisoned");
                *count += 1;
                let n = *count;
                if n == 1 {
                    Ok(SolverResult {
                        outcome: SolverOutcome::Error("parse error".to_string()),
                        stdout: "sat".to_string(),
                        stderr: "error".to_string(),
                    })
                } else {
                    Ok(SolverResult {
                        outcome: SolverOutcome::Unsat,
                        stdout: "unsat".to_string(),
                        stderr: String::new(),
                    })
                }
            }
        }

        let llm = SequenceLlm::new(
            vec![
                formula_response(),
                "The solver had an error, let me fix it.".to_string(),
                formula_response(),
                "Now the formula is correct and unsat.".to_string(),
            ],
            vec!["YES".to_string()],
        );
        let call_count = Arc::new(Mutex::new(0usize));
        let solver = ToggleSolver { call_count: call_count.clone() };

        let result = crate::agent::run_with_providers("refactoring desc", &llm, &solver)
            .await
            .expect("agent should succeed");

        assert!(!result.analysis.is_empty());
        assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
        assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
        assert_eq!(*call_count.lock().unwrap(), 2, "solver should be called twice");
    }

    #[test_log::test(tokio::test)]
    async fn test_judge_says_no_then_yes() {
        let llm = SequenceLlm::new(
            vec![
                formula_response(),
                "Analysis says unsat is correct.".to_string(),
                formula_response(),
                "Revised analysis confirms equivalence.".to_string(),
            ],
            vec!["NO".to_string(), "YES".to_string()],
        );
        let solver = FakeSolver { outcome: SolverOutcome::Unsat };

        let result = crate::agent::run_with_providers("refactoring desc", &llm, &solver)
            .await
            .expect("agent should succeed");

        assert!(!result.analysis.is_empty());
        assert_eq!(llm.primary_remaining(), 0, "all primary responses consumed");
        assert_eq!(llm.judge_remaining(), 0, "all judge responses consumed");
    }






    }