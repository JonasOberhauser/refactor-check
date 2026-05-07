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
    pub formulas: Vec<(String, SolverOutcome, String)>, // (formula, outcome, verdict)
    pub overall_equivalent: bool,
    pub open_count: usize,
    pub reasonable_sat: usize,
    pub reasonable_unsat: usize,
    pub reasonable_unknown: usize,
}