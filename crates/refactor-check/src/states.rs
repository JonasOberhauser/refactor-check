use std::sync::Arc;

use refactor_check_core::context_id::ContextId;

pub use crate::piece::CodePiece;
use crate::piece_manager::PieceManager;
use crate::result::AgentResult;
use crate::provider::{DynLlmProvider, DynSolverProvider};
use crate::smt::{SolverOutcome, SolverResult};

pub type Step = refactor_check_core::checkpoint::Step<Stage, AgentResult>;

/// The machine between two steps, and the root node of its checkpoint tree.
/// Fan-out stages keep their children inside, and every child stores its own
/// progress and result, so a snapshot taken mid-fan-out records which
/// children already finished.
pub enum Stage {
    Split(WaitForSplit),
    SplittingJudge(WaitForSplittingJudge),
    Generation(WaitForGeneration),
    Results(WaitForResults),
    Explanation(WaitForExplanation),
}

/// A stage's transition. Fan-out stages run their children first (see
/// `machine::fan_out`), so `execute` only collects what the children stored.
pub trait AlgorithmState: Send + Sync {
    fn execute(
        self,
        llm: &DynLlmProvider,
        solver: &DynSolverProvider,
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
    pub judge_feedback: Option<String>,
}

pub struct WaitForSplittingJudge {
    pub input_content: Arc<String>,
    pub pieces: Vec<Arc<CodePiece>>,
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
    pub pieces: Vec<GenerationSlot>,
}

/// One piece of a [`WaitForGeneration`] fan-out. `formula` is `None` until the
/// piece's worker has run; an empty formula means none could be extracted.
pub struct GenerationSlot {
    pub piece: Arc<CodePiece>,
    pub formula: Option<String>,
}

impl GenerationSlot {
    pub fn new(piece: Arc<CodePiece>) -> Self {
        Self { piece, formula: None }
    }
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
    /// One per SAT or UNKNOWN formula of `result`, in formula order.
    pub explanations: Vec<ExplanationSlot>,
}

impl WaitForExplanation {
    /// Contexts are handed out here, in formula order, so the explanation
    /// workers number them the same way however they are scheduled.
    pub fn new(input_content: Arc<String>, result: AgentResult) -> Self {
        let root = ContextId::root();
        let explanations = result
            .formulas
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f.outcome, SolverOutcome::Sat | SolverOutcome::Unknown))
            .map(|(formula_index, f)| ExplanationSlot {
                formula_index,
                formula: f.formula.clone(),
                outcome: f.outcome.clone(),
                ctx: Some(Box::new(root.new_child())),
                explanation: None,
            })
            .collect();
        Self { input_content, result, explanations }
    }
}

/// One formula of the [`WaitForExplanation`] fan-out; its worker stores the
/// explanation.
pub struct ExplanationSlot {
    pub formula_index: usize,
    pub formula: String,
    pub outcome: SolverOutcome,
    /// `None` once a request took it and failed; the slot is then done.
    pub ctx: Option<Box<ContextId>>,
    pub explanation: Option<String>,
}

#[derive(Debug)]
pub struct VerifiedPiece {
    pub piece: Arc<CodePiece>,
    pub formula: String,
    pub outcome: SolverOutcome,
}

impl std::fmt::Display for VerifiedPiece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{} {}] {} ({:?})", self.piece.label(), self.piece.ctx_display(), self.formula, self.outcome)
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

/// One piece of a [`WaitForResults`] fan-out. Its worker advances `phase` in
/// place; `formula` is the latest formula and `done` the outcome once reached.
pub struct FormulaBranch {
    pub piece: Arc<CodePiece>,
    pub input_content: Arc<String>,
    pub formula: String,
    pub phase: BranchPhase,
    pub retry_count: usize,
    pub done: Option<ChildDone>,
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