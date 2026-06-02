use anyhow::Result;
use async_trait::async_trait;

pub use crate::llm::Message;
pub use crate::smt::SolverResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRole {
    Splitter,
    SplittingJudge,
    Formalizer,
    Fixer,
    Judge,
}

pub struct LlmRequest {
    pub role: LlmRole,
    pub messages: Vec<Message>,
    pub piece_id: Option<u64>,
}

pub struct SolverRequest {
    pub formula: String,
    pub piece_id: Option<u64>,
}

#[async_trait]
pub trait IOProvider<I, O>: Send + Sync {
    async fn invoke(&self, input: I) -> Result<O>;
}

pub type DynLlmProvider = dyn IOProvider<LlmRequest, String>;
pub type DynSolverProvider = dyn IOProvider<SolverRequest, SolverResult>;