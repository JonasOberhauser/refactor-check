//! Reproduces the result-aggregation bug: a bug report found in an earlier
//! `ProblemAnalyzer` cycle is discarded when a later cycle terminates via
//! `FullFormalizer` returning `Step::Result` directly (all pieces closed).
//!
//! Scenario, driven by deterministic injected providers:
//!   cycle 1: two pieces -> solver Sat -> judge REASONABLE -> 2 unverified.
//!            ProblemAnalyzer: f0 -> "Found a bug..." (bug report),
//!                              f1 -> "RETRY"          (-> Restarter).
//!   cycle 2: re-verify  -> solver Unsat -> judge REASONABLE -> all closed.
//!            FullFormalizer returns Result directly (bug_reports: Vec::new()).
//!
//! The f0 bug report from cycle 1 must survive into the final result.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use deductive_check::code_piece::FunctionId;
use deductive_check::machine;
use deductive_check::piece_manager::DefaultDeductivePieceManager;
use deductive_check::provider::{
    AgentRequest, AgentResponse, CalledFunctionCode, FileSystemRequest, FileSystemResponse,
    FunctionInfo, GitRequest, GitResponse, Providers, PythonRequest, PythonResponse,
    RustAnalyzerRequest, RustAnalyzerResponse,
};
use refactor_check_core::provider::{IOProvider, LlmRequest, LlmRole, SolverRequest, WithContext};
use refactor_check_core::smt::{SolverOutcome, SolverResult};

const SPLITTER_OUT: &str =
    "```rust\nfn piece() {\n    let x = 1;\n    assert!(x > 0);\n    return;\n}\n```\n";
const SMT_OUT: &str = "```smt2\n(set-logic ALL)\n(declare-const x Int)\n(assert (> x 0))\n(check-sat)\n```\n";

// ── LLM: role-fixed responses (judge always REASONABLE) ────────────────────

struct MockLlm;

#[async_trait]
impl IOProvider<LlmRequest, WithContext<String>> for MockLlm {
    async fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let resp = match input.role {
            LlmRole::Splitter => SPLITTER_OUT.to_string(),
            LlmRole::Formalizer | LlmRole::Fixer => SMT_OUT.to_string(),
            LlmRole::Judge => "REASONABLE".to_string(),
            LlmRole::SplittingJudge | LlmRole::Analyzer => String::new(),
        };
        Ok(WithContext { value: resp, context_id: input.context_id })
    }
}

// ── Solver: scripted outcomes (Sat in cycle 1, Unsat in cycle 2) ───────────

struct MockSolver {
    outcomes: Mutex<VecDeque<SolverOutcome>>,
}

#[async_trait]
impl IOProvider<SolverRequest, WithContext<SolverResult>> for MockSolver {
    async fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let outcome = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(SolverOutcome::Unsat);
        let stdout = match &outcome {
            SolverOutcome::Unsat => "unsat".to_string(),
            SolverOutcome::Sat => "sat".to_string(),
            SolverOutcome::Unknown => "unknown".to_string(),
            SolverOutcome::Error(e) => e.clone(),
        };
        Ok(WithContext {
            value: SolverResult { outcome, stdout, stderr: String::new() },
            context_id: input.context_id,
        })
    }
}

// ── Rust-analyzer: two functions f0/f1, fixed bodies ───────────────────────

struct MockRa;

#[async_trait]
impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for MockRa {
    async fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        Ok(match input {
            RustAnalyzerRequest::ListFunctions { files, .. } => {
                let file = files.first().cloned().unwrap_or_else(|| PathBuf::from("src/lib.rs"));
                RustAnalyzerResponse::FunctionList(vec![
                    function_info(file.clone(), "f0"),
                    function_info(file, "f1"),
                ])
            }
            RustAnalyzerRequest::GetFunctionCode { function_id } => RustAnalyzerResponse::FunctionCode(format!(
                "fn {}() {{\n    let x = 1;\n    assert!(x > 0);\n}}\n",
                function_id.name
            )),
            RustAnalyzerRequest::GetCalledFunctions { .. } => {
                RustAnalyzerResponse::CalledFunctionList(Vec::new())
            }
            RustAnalyzerRequest::GetCalledFunctionCode { called } => {
                RustAnalyzerResponse::CalledFunctionCode(CalledFunctionCode {
                    code: format!("fn {}() {{}}", called.name),
                    docs: String::new(),
                })
            }
            RustAnalyzerRequest::GetFunctionDocs { .. } => {
                RustAnalyzerResponse::FunctionDocs(String::new())
            }
            RustAnalyzerRequest::GetFileContent { .. } => {
                RustAnalyzerResponse::FileContent("fn main() {}".to_string())
            }
        })
    }
}

fn function_info(file: PathBuf, name: &str) -> FunctionInfo {
    FunctionInfo {
        id: FunctionId::new(file, name, None, 1),
        body: String::new(),
        start_line: 1,
        end_line: 5,
        docs: String::new(),
        has_guarantees: true,
    }
}

// ── Git / FS / Python: fixed happy-path responses ──────────────────────────

struct MockGit;
#[async_trait]
impl IOProvider<GitRequest, GitResponse> for MockGit {
    async fn invoke(&self, input: GitRequest) -> Result<GitResponse> {
        Ok(match input {
            GitRequest::WalkRustFiles { .. } => {
                GitResponse { success: true, output: "src/lib.rs\n".to_string() }
            }
            GitRequest::CurrentCommitHash => {
                GitResponse { success: true, output: "deadbeefcafebabe1234567890abcdef12345678".to_string() }
            }
            GitRequest::FindChangedRustFiles { .. } => GitResponse { success: true, output: String::new() },
            _ => GitResponse { success: true, output: String::new() },
        })
    }
}

struct MockFs;
#[async_trait]
impl IOProvider<FileSystemRequest, FileSystemResponse> for MockFs {
    async fn invoke(&self, input: FileSystemRequest) -> Result<FileSystemResponse> {
        Ok(FileSystemResponse { path: input.dir.join(&input.filename) })
    }
}

struct MockPython;
#[async_trait]
impl IOProvider<PythonRequest, PythonResponse> for MockPython {
    async fn invoke(&self, input: PythonRequest) -> Result<PythonResponse> {
        let smtlib = if input.script.trim().is_empty() {
            "(check-sat)".to_string()
        } else {
            input.script
        };
        Ok(PythonResponse { smtlib, explanation: String::new() })
    }
}

// ── Agent: scripted (preflight OK, f0 bug, f1 RETRY) ───────────────────────

struct MockAgent {
    responses: Mutex<VecDeque<(bool, String)>>,
}

#[async_trait]
impl IOProvider<AgentRequest, AgentResponse> for MockAgent {
    async fn invoke(&self, _input: AgentRequest) -> Result<AgentResponse> {
        let (success, stdout) = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((true, "No bug found.".to_string()));
        Ok(AgentResponse { success, stdout })
    }
}

// ── Bundle ─────────────────────────────────────────────────────────────────

struct Mocks {
    llm: MockLlm,
    solver: MockSolver,
    ra: MockRa,
    git: MockGit,
    fs: MockFs,
    python: MockPython,
    agent: MockAgent,
}

impl Mocks {
    fn providers(&self) -> Providers<'_> {
        Providers {
            llm: &self.llm,
            solver: &self.solver,
            rust_analyzer: &self.ra,
            git: &self.git,
            filesystem: &self.fs,
            python: &self.python,
            agent: &self.agent,
        }
    }
}

#[tokio::test]
async fn bug_report_survives_restart_cycle() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git marker");

    let mocks = Mocks {
        llm: MockLlm,
        solver: MockSolver {
            outcomes: Mutex::new(VecDeque::from([
                SolverOutcome::Unsat, // preflight (outcome ignored)
                SolverOutcome::Sat,   // cycle 1, piece f0 -> unverified
                SolverOutcome::Sat,   // cycle 1, piece f1 -> unverified
                SolverOutcome::Unsat, // cycle 2, piece f0 -> closed
                SolverOutcome::Unsat, // cycle 2, piece f1 -> closed
            ])),
        },
        ra: MockRa,
        git: MockGit,
        fs: MockFs,
        python: MockPython,
        agent: MockAgent {
            responses: Mutex::new(VecDeque::from([
                (true, "OK".to_string()),                       // preflight
                (true, "Found a bug: x can be 0.".to_string()),  // cycle 1 f0 -> bug report
                (true, "RETRY".to_string()),                     // cycle 1 f1 -> restart
            ])),
        },
    };

    let pm = DefaultDeductivePieceManager::default();
    let result = machine::run(
        project.path().to_str().expect("utf8 temp path"),
        &mocks.providers(),
        &pm,
    )
    .await
    .expect("machine ran");

    assert_eq!(
        result.bug_reports.len(),
        1,
        "the f0 bug report from cycle 1 must survive the restart into the final result; \
         got bug_reports={:?} (closed={}, unverified={})",
        result.bug_reports,
        result.closed_pieces.len(),
        result.unverified_pieces.len(),
    );
    assert!(
        result.bug_reports[0].description.contains("x can be 0"),
        "preserved bug report should be f0's: {:?}",
        result.bug_reports[0]
    );
}
