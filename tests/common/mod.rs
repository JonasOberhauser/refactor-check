use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

use refactor_check::llm::Message;
use refactor_check::provider::{LlmProvider, LlmRole, SolverProvider};
use refactor_check::smt::{SolverOutcome, SolverResult};

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
            anyhow::bail!("SequenceLlm: no more {role:?} responses available");
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
