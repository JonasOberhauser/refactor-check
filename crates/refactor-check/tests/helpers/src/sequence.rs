use std::sync::{Arc, Mutex};

use anyhow::Result;

use async_trait::async_trait;
use refactor_check::provider::{IOProvider, LlmRequest, LlmRole, SolverRequest, WithContext};
use refactor_check::smt::{SolverOutcome, SolverResult};

pub struct SequenceLlm {
    formalizer: Arc<Mutex<Vec<String>>>,
    fixer: Arc<Mutex<Vec<String>>>,
    judge: Arc<Mutex<Vec<String>>>,
    splitter: Arc<Mutex<Vec<String>>>,
    splitting_judge: Arc<Mutex<Vec<String>>>,
    analyzer: Arc<Mutex<Vec<String>>>,
}

impl SequenceLlm {
    pub fn new(
        formalizer: Vec<String>,
        fixer: Vec<String>,
        judge: Vec<String>,
        splitter: Vec<String>,
        splitting_judge: Vec<String>,
        analyzer: Vec<String>,
    ) -> Self {
        Self {
            formalizer: Arc::new(Mutex::new(formalizer)),
            fixer: Arc::new(Mutex::new(fixer)),
            judge: Arc::new(Mutex::new(judge)),
            splitter: Arc::new(Mutex::new(splitter)),
            splitting_judge: Arc::new(Mutex::new(splitting_judge)),
            analyzer: Arc::new(Mutex::new(analyzer)),
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
            analyzer: Arc::new(Mutex::new(Vec::new())),
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
impl IOProvider<LlmRequest, WithContext<String>> for SequenceLlm {
    async fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let LlmRequest { role, context_id, .. } = input;
        let queue = match role {
            LlmRole::Splitter => &self.splitter,
            LlmRole::SplittingJudge => &self.splitting_judge,
            LlmRole::Formalizer => &self.formalizer,
            LlmRole::Fixer => &self.fixer,
            LlmRole::Judge => &self.judge,
            LlmRole::Analyzer => &self.analyzer,
        };
        let mut responses = queue.lock().expect("lock poisoned");
        if responses.is_empty() {
            anyhow::bail!("SequenceLlm: no more {:?} responses available", role);
        }
        Ok(WithContext { value: responses.remove(0), context_id })
    }
}

pub struct FakeSolver {
    pub outcome: SolverOutcome,
}

#[async_trait]
impl IOProvider<SolverRequest, WithContext<SolverResult>> for FakeSolver {
    async fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let SolverRequest { context_id, .. } = input;
        Ok(WithContext {
            value: SolverResult {
                outcome: self.outcome.clone(),
                stdout: format!("{:?}", self.outcome).to_lowercase(),
                stderr: String::new(),
            },
            context_id,
        })
    }
}
