//! The handle architecture: piece verification fan-outs run under
//! `Handle::fanout` — one DEDICATED thread per piece. This test proves the
//! parallelism is real: with a multi-function project, the solver mock
//! observes more than one distinct OS thread, and the run still produces
//! the same closed result as the sequential semantics.
#![allow(clippy::unwrap_used, clippy::panic, unused_results)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;

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

struct MockLlm;
impl IOProvider<LlmRequest, WithContext<String>> for MockLlm {
    fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let resp = match input.role {
            LlmRole::Splitter => SPLITTER_OUT.to_string(),
            LlmRole::Formalizer | LlmRole::Fixer => SMT_OUT.to_string(),
            LlmRole::Judge => "REASONABLE".to_string(),
            LlmRole::SplittingJudge | LlmRole::Analyzer => String::new(),
        };
        Ok(WithContext { value: resp, context_id: input.context_id })
    }
}

/// Solver mock that records the OS thread of every invocation: piece
/// children run on dedicated threads, so a multi-piece batch must show up
/// as several distinct thread ids.
struct ThreadRecordingSolver {
    threads: Mutex<Vec<std::thread::ThreadId>>,
}
impl IOProvider<SolverRequest, WithContext<SolverResult>> for ThreadRecordingSolver {
    fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        self.threads.lock().unwrap().push(std::thread::current().id());
        Ok(WithContext {
            value: SolverResult {
                outcome: SolverOutcome::Unsat,
                stdout: "unsat".to_string(),
                stderr: String::new(),
            },
            context_id: input.context_id,
        })
    }
}

struct MockRa;
impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for MockRa {
    fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        Ok(match input {
            RustAnalyzerRequest::ListFunctions { files, .. } => {
                let file = files.first().cloned().unwrap_or_else(|| PathBuf::from("src/lib.rs"));
                RustAnalyzerResponse::FunctionList(vec![
                    function_info(file.clone(), "f0"),
                    function_info(file.clone(), "f1"),
                    function_info(file, "f2"),
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

struct MockGit;
impl IOProvider<GitRequest, GitResponse> for MockGit {
    fn invoke(&self, input: GitRequest) -> Result<GitResponse> {
        Ok(match input {
            GitRequest::WalkRustFiles { .. } => {
                GitResponse { success: true, output: "src/lib.rs\n".to_string() }
            }
            GitRequest::CurrentCommitHash => {
                GitResponse { success: true, output: "deadbeef".to_string() }
            }
            _ => GitResponse { success: true, output: String::new() },
        })
    }
}

struct MockFs;
impl IOProvider<FileSystemRequest, FileSystemResponse> for MockFs {
    fn invoke(&self, input: FileSystemRequest) -> Result<FileSystemResponse> {
        Ok(FileSystemResponse { path: input.dir.join(&input.filename) })
    }
}

struct MockPython;
impl IOProvider<PythonRequest, PythonResponse> for MockPython {
    fn invoke(&self, input: PythonRequest) -> Result<PythonResponse> {
        Ok(PythonResponse { smtlib: input.script, explanation: String::new() })
    }
}

struct MockAgent {
    responses: Mutex<VecDeque<(bool, String)>>,
}
impl IOProvider<AgentRequest, AgentResponse> for MockAgent {
    fn invoke(&self, _input: AgentRequest) -> Result<AgentResponse> {
        let (success, stdout) = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((true, "No bug found.".to_string()));
        Ok(AgentResponse { success, stdout })
    }
}

#[test]
fn piece_fanout_runs_on_dedicated_threads() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git marker");

    let llm = MockLlm;
    let solver = ThreadRecordingSolver { threads: Mutex::new(Vec::new()) };
    let agent = MockAgent { responses: Mutex::new(VecDeque::new()) };
    let providers = Providers {
        llm: &llm,
        solver: &solver,
        rust_analyzer: &MockRa,
        git: &MockGit,
        filesystem: &MockFs,
        python: &MockPython,
        agent: &agent,
    };
    let pm = DefaultDeductivePieceManager::default();

    let result = machine::run(
        project.path().to_str().expect("utf8 temp path"),
        &providers,
        &pm,
    )
    .expect("machine run");

    // Three functions -> three pieces, each closing on its first check.
    assert_eq!(result.closed_pieces.len(), 3, "all pieces close");
    assert!(result.bug_reports.is_empty(), "no bug reports expected");

    // The fan-out is REAL: each piece child ran on its own dedicated
    // thread, so the solver observed more than one distinct OS thread
    // (the parent thread is not among them).
    let threads = solver.threads.lock().unwrap().clone();
    let mut distinct: std::collections::HashSet<std::thread::ThreadId> =
        threads.into_iter().collect();
    distinct.remove(&std::thread::current().id());
    assert!(
        distinct.len() >= 2,
        "piece fan-out must run on dedicated threads, saw only {} distinct",
        distinct.len()
    );
}
