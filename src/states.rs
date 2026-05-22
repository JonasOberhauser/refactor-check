use std::sync::Arc;

use async_trait::async_trait;

use crate::piece_manager::PieceManager;
use crate::provider::{AgentResult, LlmProvider, SolverProvider};
use crate::smt::{SolverOutcome, SolverResult};

#[derive(Debug, PartialEq, Eq)]
pub struct CodePiece {
    id: u64,
    label: String,
    before: String,
    after: String,
}

impl CodePiece {
    pub(crate) fn with_id(id: u64, label: &str, before: &str, after: &str) -> Self {
        Self {
            id,
            label: label.to_string(),
            before: before.to_string(),
            after: after.to_string(),
        }
    }

    pub fn id(&self) -> u64 { self.id }
    pub fn label(&self) -> &str { &self.label }
    pub fn before(&self) -> &str { &self.before }
    pub fn after(&self) -> &str { &self.after }
}

pub enum Step {
    State(Box<dyn AlgorithmState>),
    Result(AgentResult),
}

#[async_trait]
pub trait AlgorithmState: Send + Sync {
    async fn execute(
        self: Box<Self>,
        llm: &dyn LlmProvider,
        solver: &dyn SolverProvider,
        pm: &dyn PieceManager,
    ) -> anyhow::Result<Step>;
}

pub enum InsistState {
    Idle,
    Insisting { last_response: String, attempt: usize },
}

impl InsistState {
    pub fn attempt(&self) -> usize {
        match self {
            InsistState::Idle => 0,
            InsistState::Insisting { attempt, .. } => *attempt,
        }
    }
}

pub struct WaitForSplit {
    pub input_content: Arc<String>,
    pub pieces_to_resplit: Vec<(Arc<CodePiece>, String)>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub split_depth: u32,
}

pub struct WaitForGeneration {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub insist: InsistState,
    pub pieces: Vec<Arc<CodePiece>>,
}

pub struct WaitForResults {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub branches: Vec<FormulaBranch>,
    pub split_depth: u32,
}

pub struct WaitForExplanation {
    pub input_content: Arc<String>,
    pub result: AgentResult,
}

#[derive(Debug)]
pub struct VerifiedPiece {
    pub piece: Arc<CodePiece>,
    pub formula: String,
    pub outcome: SolverOutcome,
}

impl std::fmt::Display for VerifiedPiece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{} #{}] {} ({:?})", self.piece.label(), self.piece.id(), self.formula, self.outcome)
    }
}

#[derive(Debug)]
pub struct OpenItem {
    pub piece: Arc<CodePiece>,
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
    pub piece: Arc<CodePiece>,
    pub input_content: Arc<String>,
    pub formula: String,
    pub phase: BranchPhase,
    pub retry_count: usize,
}

#[derive(Clone)]
pub enum BranchPhase {
    NeedFormula {
        feedback: Option<String>,
        solver_stdout: String,
        solver_stderr: String,
        insist_pending: bool,
        insist_attempt: usize,
        last_response: Option<String>,
    },
    WaitForSolver {
        formula: String,
    },
    WaitForJudge {
        formula: String,
        solver_result: SolverResult,
    },
}

pub enum BranchFromNeedFormula {
    Proceed(String),
    Insist,
    Exhausted(String),
}

pub enum BranchFromSolver {
    Judge(String, SolverResult),
    Error(String, SolverResult),
    Resplit(String, SolverResult),
}

pub enum BranchFromJudge {
    Verified(VerifiedPiece),
    Retry {
        formula: String,
        feedback: String,
        solver_stdout: String,
        solver_stderr: String,
    },
    Exhausted {
        formula: String,
        piece: Arc<CodePiece>,
        feedback: String,
        solver_stdout: String,
        solver_stderr: String,
    },
}

pub enum ChildDone {
    Verified(VerifiedPiece),
    Open(OpenItem),
    NeedsResplit {
        piece: Arc<CodePiece>,
        formula: String,
        reason: String,
    },
}

#[derive(Default)]
pub struct OutcomeCounts {
    pub sat: usize,
    pub unsat: usize,
    pub unknown: usize,
}

impl OutcomeCounts {
    pub fn from_verified(verified: &[VerifiedPiece]) -> Self {
        let mut c = Self::default();
        for v in verified {
            match v.outcome {
                SolverOutcome::Sat => c.sat += 1,
                SolverOutcome::Unsat => c.unsat += 1,
                SolverOutcome::Unknown => c.unknown += 1,
                _ => {}
            }
        }
        c
    }

    pub fn needs_explanation(&self) -> bool {
        self.sat > 0 || self.unknown > 0
    }

    pub fn overall_equivalent(&self, open_count: usize) -> bool {
        open_count == 0 && self.sat == 0 && self.unknown == 0
    }
}