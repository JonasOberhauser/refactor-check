use std::sync::{Arc, Mutex};

use anyhow::Result;

use async_trait::async_trait;
use refactor_check::provider::{IOProvider, LlmRequest, LlmRole, SolverRequest, WithContext};
use refactor_check::smt::{SolverOutcome, SolverResult};

pub struct LogReplayLlm {
    formalizer: Arc<Mutex<Vec<String>>>,
    fixer: Arc<Mutex<Vec<String>>>,
    judge: Arc<Mutex<Vec<String>>>,
    splitter: Arc<Mutex<Vec<String>>>,
    splitting_judge: Arc<Mutex<Vec<String>>>,
    analyzer: Arc<Mutex<Vec<String>>>,
}

impl Default for LogReplayLlm {
    fn default() -> Self {
        Self::new()
    }
}

impl LogReplayLlm {
    pub fn new() -> Self {
        Self {
            formalizer: Arc::new(Mutex::new(Vec::new())),
            fixer: Arc::new(Mutex::new(Vec::new())),
            judge: Arc::new(Mutex::new(Vec::new())),
            splitter: Arc::new(Mutex::new(Vec::new())),
            splitting_judge: Arc::new(Mutex::new(Vec::new())),
            analyzer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn take_first(q: &Arc<Mutex<Vec<String>>>, role: &str) -> Result<String> {
        let mut q = q.lock().expect("lock poisoned");
        if q.is_empty() {
            anyhow::bail!("LogReplayLlm: no more {role} responses");
        }
        Ok(q.remove(0))
    }

    pub fn splitter_push(&mut self, response: String) {
        self.splitter.lock().unwrap().push(response);
    }

    pub fn splitting_judge_push(&mut self, response: String) {
        self.splitting_judge.lock().unwrap().push(response);
    }

    pub fn formalizer_push(&mut self, response: String) {
        self.formalizer.lock().unwrap().push(response);
    }

    pub fn fixer_push(&mut self, response: String) {
        self.fixer.lock().unwrap().push(response);
    }

    pub fn judge_push(&mut self, response: String) {
        self.judge.lock().unwrap().push(response);
    }

    pub fn analyzer_push(&mut self, response: String) {
        self.analyzer.lock().unwrap().push(response);
    }
}

#[async_trait]
impl IOProvider<LlmRequest, WithContext<String>> for LogReplayLlm {
    async fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let LlmRequest { role, context_id, .. } = input;
        let value = match role {
            LlmRole::Splitter => Self::take_first(&self.splitter, "Splitter")?,
            LlmRole::SplittingJudge => Self::take_first(&self.splitting_judge, "SplittingJudge")?,
            LlmRole::Formalizer => Self::take_first(&self.formalizer, "Formalizer")?,
            LlmRole::Fixer => Self::take_first(&self.fixer, "Fixer")?,
            LlmRole::Judge => Self::take_first(&self.judge, "Judge")?,
            LlmRole::Analyzer => Self::take_first(&self.analyzer, "Analyzer")?,
        };
        Ok(WithContext { value, context_id })
    }
}

pub struct LogReplaySolver {
    runs: Arc<Mutex<Vec<SolverResult>>>,
}

impl Default for LogReplaySolver {
    fn default() -> Self {
        Self::new()
    }
}

impl LogReplaySolver {
    pub fn new() -> Self {
        Self {
            runs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn push(&mut self, outcome: SolverOutcome, stdout: String, stderr: String) {
        self.runs.lock().unwrap().push(SolverResult {
            outcome,
            stdout,
            stderr,
        });
    }
}

#[async_trait]
impl IOProvider<SolverRequest, WithContext<SolverResult>> for LogReplaySolver {
    async fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let SolverRequest { context_id, .. } = input;
        let mut q = self.runs.lock().expect("lock poisoned");
        if q.is_empty() {
            anyhow::bail!("LogReplaySolver: no more outcomes");
        }
        Ok(WithContext { value: q.remove(0), context_id })
    }
}
