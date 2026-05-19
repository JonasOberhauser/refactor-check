use std::sync::Arc;

use crate::provider::AgentResult;
use crate::smt::{SolverOutcome, SolverResult};

pub enum AlgorithmState {
    Idle(Idle),
    WaitForGeneration(WaitForGeneration),
    WaitForResults(WaitForResults),
    WaitForExplanation(WaitForExplanation),
    Done(AgentResult),
}

pub struct Idle;

pub struct WaitForGeneration {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub insist_pending: bool,
    pub insist_attempt: usize,
    pub last_response: Option<String>,
}

pub struct WaitForResults {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub branches: Vec<FormulaBranch>,
}

pub struct WaitForExplanation {
    pub input_content: Arc<String>,
    pub result: AgentResult,
}

pub enum TransitionFromGeneration {
    Results(WaitForResults),
    Insist(WaitForGeneration),
}

pub enum TransitionFromResults {
    Generation(WaitForGeneration),
    Explain(WaitForExplanation),
    Done(AgentResult),
}

pub enum TransitionFromExplanation {
    Done(AgentResult),
}

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

pub struct FormulaBranch {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub formula_id: String,
    pub state: BranchState,
    pub retry_count: usize,
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