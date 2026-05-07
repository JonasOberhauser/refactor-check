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

pub struct FormulaResult {
    pub formula: String,
    pub outcome: SolverOutcome,
    pub verdict: String,
    pub explanation: Option<String>,
}

pub struct AgentResult {
    pub formulas: Vec<FormulaResult>,
    pub overall_equivalent: bool,
    pub open_count: usize,
    pub reasonable_sat: usize,
    pub reasonable_unsat: usize,
    pub reasonable_unknown: usize,
}