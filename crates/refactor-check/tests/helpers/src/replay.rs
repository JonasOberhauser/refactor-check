use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use refactor_check::llm::Message;
use refactor_check::provider::{LlmProvider, LlmRole, SolverProvider};
use refactor_check::smt::{SolverOutcome, SolverResult};

pub struct LogReplayLlm {
    formalizer: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    fixer: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    judge: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    splitter: Arc<Mutex<Vec<String>>>,
    splitting_judge: Arc<Mutex<Vec<String>>>,
}

impl Default for LogReplayLlm {
    fn default() -> Self {
        Self::new()
    }
}

impl LogReplayLlm {
    pub fn new() -> Self {
        Self {
            formalizer: Arc::new(Mutex::new(HashMap::new())),
            fixer: Arc::new(Mutex::new(HashMap::new())),
            judge: Arc::new(Mutex::new(HashMap::new())),
            splitter: Arc::new(Mutex::new(Vec::new())),
            splitting_judge: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn splitter_push(&mut self, response: String) {
        self.splitter.lock().unwrap().push(response);
    }

    pub fn splitting_judge_push(&mut self, response: String) {
        self.splitting_judge.lock().unwrap().push(response);
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
        piece_id: Option<u64>,
    ) -> Result<String> {
        match role {
            LlmRole::Splitter => {
                let mut q = self.splitter.lock().expect("lock poisoned");
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more Splitter responses");
                }
                Ok(q.remove(0))
            }
            LlmRole::SplittingJudge => {
                let mut q = self.splitting_judge.lock().expect("lock poisoned");
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more SplittingJudge responses");
                }
                Ok(q.remove(0))
            }
            LlmRole::Formalizer => {
                let pid = piece_id.expect("formalizer must have piece_id");
                let mut map = self.formalizer.lock().expect("lock poisoned");
                let q = map.get_mut(&pid)
                    .ok_or_else(|| anyhow::anyhow!("LogReplayLlm: no Formalizer entry for piece_id={pid}"))?;
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more Formalizer responses for piece_id={pid}");
                }
                Ok(q.remove(0))
            }
            LlmRole::Fixer => {
                let pid = piece_id.expect("fixer must have piece_id");
                let mut map = self.fixer.lock().expect("lock poisoned");
                let q = map.get_mut(&pid)
                    .ok_or_else(|| anyhow::anyhow!("LogReplayLlm: no Fixer entry for piece_id={pid}"))?;
                if q.is_empty() {
                    anyhow::bail!("LogReplayLlm: no more Fixer responses for piece_id={pid}");
                }
                Ok(q.remove(0))
            }
            LlmRole::Judge => {
                let pid = piece_id.expect("judge must have piece_id");
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

pub struct LogReplaySolver {
    runs: Arc<Mutex<HashMap<u64, Vec<SolverResult>>>>,
}

impl Default for LogReplaySolver {
    fn default() -> Self {
        Self::new()
    }
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
    async fn run(&self, _formula: &str, piece_id: Option<u64>) -> Result<SolverResult> {
        let pid = piece_id.expect("solver must have piece_id");
        let mut map = self.runs.lock().expect("lock poisoned");
        let q = map.get_mut(&pid)
            .ok_or_else(|| anyhow::anyhow!("LogReplaySolver: no entry for piece_id={pid}"))?;
        if q.is_empty() {
            anyhow::bail!("LogReplaySolver: no more outcomes for piece_id={pid}");
        }
        Ok(q.remove(0))
    }
}