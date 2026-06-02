use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use refactor_check::provider::{IOProvider, LlmRequest, LlmRole, SolverRequest};
use refactor_check::smt::{SolverOutcome, SolverResult};

pub struct SequenceLlm {
    formalizer: Arc<Mutex<Vec<String>>>,
    fixer: Arc<Mutex<Vec<String>>>,
    judge: Arc<Mutex<Vec<String>>>,
    splitter: Arc<Mutex<Vec<String>>>,
    splitting_judge: Arc<Mutex<Vec<String>>>,
}

impl SequenceLlm {
    pub fn new(
        formalizer: Vec<String>,
        fixer: Vec<String>,
        judge: Vec<String>,
        splitter: Vec<String>,
        splitting_judge: Vec<String>,
    ) -> Self {
        Self {
            formalizer: Arc::new(Mutex::new(formalizer)),
            fixer: Arc::new(Mutex::new(fixer)),
            judge: Arc::new(Mutex::new(judge)),
            splitter: Arc::new(Mutex::new(splitter)),
            splitting_judge: Arc::new(Mutex::new(splitting_judge)),
        }
    }

    pub fn with_accepting_judge(
        formalizer: Vec<String>,
        fixer: Vec<String>,
        judge: Vec<String>,
        splitter: Vec<String>,
    ) -> Self {
        let split_count = splitter.len();
        Self {
            formalizer: Arc::new(Mutex::new(formalizer)),
            fixer: Arc::new(Mutex::new(fixer)),
            judge: Arc::new(Mutex::new(judge)),
            splitter: Arc::new(Mutex::new(splitter)),
            splitting_judge: Arc::new(Mutex::new(vec!["REASONABLE".to_string(); split_count])),
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
impl IOProvider<LlmRequest, String> for SequenceLlm {
    async fn invoke(&self, input: LlmRequest) -> Result<String> {
        let queue = match input.role {
            LlmRole::Splitter => &self.splitter,
            LlmRole::SplittingJudge => &self.splitting_judge,
            LlmRole::Formalizer => &self.formalizer,
            LlmRole::Fixer => &self.fixer,
            LlmRole::Judge => &self.judge,
        };
        let mut responses = queue.lock().expect("lock poisoned");
        if responses.is_empty() {
            anyhow::bail!("SequenceLlm: no more {:?} responses available", input.role);
        }
        Ok(responses.remove(0))
    }
}

pub struct FakeSolver {
    pub outcome: SolverOutcome,
}

#[async_trait]
impl IOProvider<SolverRequest, SolverResult> for FakeSolver {
    async fn invoke(&self, _input: SolverRequest) -> Result<SolverResult> {
        Ok(SolverResult {
            outcome: self.outcome.clone(),
            stdout: format!("{:?}", self.outcome).to_lowercase(),
            stderr: String::new(),
        })
    }
}
