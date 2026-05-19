use std::sync::Arc;

use crate::provider::AgentResult;
use crate::smt::{SolverOutcome, SolverResult};

pub enum AlgorithmState {
    Idle,
    WaitForGeneration(WaitForGeneration),
    WaitForResults(WaitForResults),
    WaitForExplanation(WaitForExplanation),
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

pub struct WaitForGeneration {
    pub input_content: Arc<String>,
    pub verified: Vec<VerifiedPiece>,
    pub open: Vec<OpenItem>,
    pub iteration: usize,
    pub insist: InsistState,
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
    FanOut(Vec<String>),
    Exhausted(String),
}

pub enum BranchFromSolver {
    Judge(String, SolverResult),
    Error(String, SolverResult),
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
        feedback: String,
        solver_stdout: String,
        solver_stderr: String,
    },
}

pub enum ChildDone {
    Verified(VerifiedPiece),
    Open(OpenItem),
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