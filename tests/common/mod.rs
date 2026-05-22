use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use refactor_check::llm::Message;
use refactor_check::piece_manager::DefaultPieceManager;
use refactor_check::provider::{LlmProvider, LlmRole, SolverProvider};
use refactor_check::smt::{SolverOutcome, SolverResult};
use refactor_check::states::CodePiece;

// ── SequenceLlm ──────────────────────────────────────────────────────

pub struct SequenceLlm {
    formalizer: Arc<Mutex<Vec<String>>>,
    fixer: Arc<Mutex<Vec<String>>>,
    judge: Arc<Mutex<Vec<String>>>,
    splitter: Arc<Mutex<Vec<String>>>,
}

#[allow(dead_code)]
impl SequenceLlm {
    pub fn new(
        formalizer: Vec<String>,
        fixer: Vec<String>,
        judge: Vec<String>,
        splitter: Vec<String>,
    ) -> Self {
        Self {
            formalizer: Arc::new(Mutex::new(formalizer)),
            fixer: Arc::new(Mutex::new(fixer)),
            judge: Arc::new(Mutex::new(judge)),
            splitter: Arc::new(Mutex::new(splitter)),
        }
    }

    pub fn formalizer_remaining(&self) -> usize {
        self.formalizer.lock().expect("lock poisoned").len()
    }

    pub fn fixer_remaining(&self) -> usize {
        self.fixer.lock().expect("lock poisoned").len()
    }

    pub fn judge_remaining(&self) -> usize {
        self.judge.lock().expect("lock poisoned").len()
    }
}

#[async_trait]
impl LlmProvider for SequenceLlm {
    async fn chat(
        &self,
        role: LlmRole,
        _messages: Vec<Message>,
        _piece: Option<&refactor_check::states::CodePiece>,
    ) -> Result<String> {
        let queue = match role {
            LlmRole::Splitter => &self.splitter,
            LlmRole::Formalizer => &self.formalizer,
            LlmRole::Fixer => &self.fixer,
            LlmRole::Judge => &self.judge,
        };
        let mut responses = queue.lock().expect("lock poisoned");
        if responses.is_empty() {
            anyhow::bail!("SequenceLlm: no more {role:?} responses available");
        }
        Ok(responses.remove(0))
    }
}

// ── FakeSolver ───────────────────────────────────────────────────────

pub struct FakeSolver {
    pub outcome: SolverOutcome,
}

#[async_trait]
impl SolverProvider for FakeSolver {
    async fn run(&self, _formula: &str, _piece: Option<&CodePiece>) -> Result<SolverResult> {
        Ok(SolverResult {
            outcome: self.outcome.clone(),
            stdout: format!("{:?}", self.outcome).to_lowercase(),
            stderr: String::new(),
        })
    }
}

// ── LogReplayLlm (per-pieceid message slices) ────────────────────────

pub struct LogReplayLlm {
    formalizer: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    fixer: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    judge: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    splitter: Arc<Mutex<Vec<String>>>,
}

impl LogReplayLlm {
    pub fn new() -> Self {
        Self {
            formalizer: Arc::new(Mutex::new(HashMap::new())),
            fixer: Arc::new(Mutex::new(HashMap::new())),
            judge: Arc::new(Mutex::new(HashMap::new())),
            splitter: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn splitter_push(&mut self, response: String) {
        self.splitter.lock().unwrap().push(response);
    }

    pub fn formalizer_push(&mut self, piece_id: u64, response: String) {
        self.formalizer.lock().unwrap().entry(piece_id).or_default().push(response);
    }

    pub fn fixer_push(&mut self, piece_id: u64, response: String) {
        self.fixer.lock().unwrap().entry(piece_id).or_default().push(response);
    }

    pub fn judge_push(&mut self, piece_id: u64, response: String) {
        self.judge.lock().unwrap().entry(piece_id).or_default().push(response);
    }
}

#[async_trait]
impl LlmProvider for LogReplayLlm {
    async fn chat(
        &self,
        role: LlmRole,
        _messages: Vec<Message>,
        piece: Option<&refactor_check::states::CodePiece>,
    ) -> Result<String> {
        match role {
            LlmRole::Splitter => {
                let mut q = self.splitter.lock().expect("lock poisoned");
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more Splitter responses");
                }
                Ok(q.remove(0))
            }
            LlmRole::Formalizer => {
                let pid = piece.expect("formalizer must have piece_id").id();
                let mut map = self.formalizer.lock().expect("lock poisoned");
                let q = map.get_mut(&pid)
                    .ok_or_else(|| anyhow::anyhow!("LogReplayLlm: no Formalizer entry for piece_id={pid}"))?;
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more Formalizer responses for piece_id={pid}");
                }
                Ok(q.remove(0))
            }
            LlmRole::Fixer => {
                let pid = piece.expect("fixer must have piece_id").id();
                let mut map = self.fixer.lock().expect("lock poisoned");
                let q = map.get_mut(&pid)
                    .ok_or_else(|| anyhow::anyhow!("LogReplayLlm: no Fixer entry for piece_id={pid}"))?;
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more Fixer responses for piece_id={pid}");
                }
                Ok(q.remove(0))
            }
            LlmRole::Judge => {
                let pid = piece.expect("judge must have piece_id").id();
                let mut map = self.judge.lock().expect("lock poisoned");
                let q = map.get_mut(&pid)
                    .ok_or_else(|| anyhow::anyhow!("LogReplayLlm: no Judge entry for piece_id={pid}"))?;
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more Judge responses for piece_id={pid}");
                }
                Ok(q.remove(0))
            }
        }
    }
}

// ── LogReplaySolver (per-pieceid outcome slices) ─────────────────────

pub struct LogReplaySolver {
    runs: Arc<Mutex<HashMap<u64, Vec<SolverResult>>>>,
}

impl LogReplaySolver {
    pub fn new() -> Self {
        Self {
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn push(&mut self, piece_id: u64, outcome: SolverOutcome, stdout: String, stderr: String) {
        self.runs.lock().unwrap().entry(piece_id).or_default().push(SolverResult {
            outcome,
            stdout,
            stderr,
        });
    }
}

#[async_trait]
impl SolverProvider for LogReplaySolver {
    async fn run(&self, _formula: &str, piece: Option<&CodePiece>) -> Result<SolverResult> {
        let pid = piece.expect("solver must have piece_id").id();
        let mut map = self.runs.lock().expect("lock poisoned");
        let q = map.get_mut(&pid)
            .ok_or_else(|| anyhow::anyhow!("LogReplaySolver: no entry for piece_id={pid}"))?;
        if q.is_empty() {
            anyhow::bail!("LogReplaySolver: no more outcomes for piece_id={pid}");
        }
        Ok(q.remove(0))
    }
}

// ── test_pm ──────────────────────────────────────────────────────────

pub fn test_pm() -> DefaultPieceManager {
    DefaultPieceManager::new()
}
