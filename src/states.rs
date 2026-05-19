use std::sync::Arc;

use crate::llm::LlmConfig;
use crate::provider::AgentResult;
use crate::smt::{SolverOutcome, SolverResult};

// ===== Global state enum =====

pub enum AlgorithmState {
    Idle(Idle),
    WaitForGeneration(WaitForGeneration),
    WaitForResults(WaitForResults),
    WaitForExplanation(WaitForExplanation),
    Done(AgentResult),
}

// ===== Global state structs =====

pub struct Idle {
    pub config: AgentConfig,
}

pub struct WaitForGeneration {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub insist_pending: bool,
    pub insist_attempt: usize,
    pub config: AgentConfig,
}

pub struct WaitForResults {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub branches: Vec<FormulaBranch>,
    pub config: AgentConfig,
}

pub struct WaitForExplanation {
    pub input_content: Arc<String>,
    pub result: AgentResult,
    pub config: AgentConfig,
}

// ===== Transition result enums =====

pub enum TransitionFromGeneration {
    Results(WaitForResults),
    Insist(WaitForGeneration),
    Exhausted,
}

pub enum TransitionFromResults {
    Generation(WaitForGeneration),
    Explain(WaitForExplanation),
    Done(AgentResult),
}

pub enum TransitionFromExplanation {
    Done(AgentResult),
}

// ===== Data carried through states =====

#[derive(Debug, Clone)]
pub struct VerifiedPiece {
    pub formula: String,
    pub outcome: SolverOutcome,
}

#[derive(Debug, Clone)]
pub struct OpenItem {
    pub formula: String,
    pub reason: String,
    pub solver_stdout: String,
    pub solver_stderr: String,
}

#[derive(Clone)]
pub enum JudgeVerdict {
    Reasonable,
    Retry(String),
}

// ===== Branch types =====

pub struct FormulaBranch {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub region: Region,
    pub state: BranchState,
    pub retry_count: usize,
    pub config: AgentConfig,
}

#[derive(Clone)]
pub enum Region {
    Global,
    Formula(String),
}

#[derive(Clone)]
pub enum BranchState {
    NeedFormula {
        feedback: Option<String>,
        solver_stdout: String,
        solver_stderr: String,
    },
    WaitForSolver {
        formula: String,
    },
    WaitForJudge {
        formula: String,
        solver_result: SolverResult,
    },
}

pub enum ChildDone {
    Verified(VerifiedPiece),
    Open(OpenItem),
}

// ===== AgentConfig =====

pub struct AgentConfig {
    pub llm_config: LlmConfig,
    pub solver_path: String,
    pub solver_args: Vec<String>,
    pub solver_timeout_secs: u64,
}

impl Clone for AgentConfig {
    fn clone(&self) -> Self {
        Self {
            llm_config: self.llm_config.clone(),
            solver_path: self.solver_path.clone(),
            solver_args: self.solver_args.clone(),
            solver_timeout_secs: self.solver_timeout_secs,
        }
    }
}
