use anyhow::Result;
use async_trait::async_trait;

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
    pub formula: String,
    pub solver_outcome: SolverOutcome,
    pub solver_stdout: String,
}