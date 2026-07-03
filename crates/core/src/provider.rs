use crate::context_id::ContextId;

pub use crate::llm::Message;
pub use crate::smt::SolverResult;
pub use servyi_ioprovider::IOProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRole {
    Splitter,
    SplittingJudge,
    Formalizer,
    Fixer,
    Judge,
    Analyzer,
}

pub struct LlmRequest {
    pub role: LlmRole,
    pub messages: Vec<Message>,
    pub context_id: Box<ContextId>,
}

pub struct SolverRequest {
    pub formula: String,
    pub context_id: Box<ContextId>,
}

pub struct WithContext<T> {
    pub value: T,
    pub context_id: Box<ContextId>,
}

pub type DynLlmProvider = dyn IOProvider<LlmRequest, WithContext<String>>;
pub type DynSolverProvider = dyn IOProvider<SolverRequest, WithContext<SolverResult>>;
