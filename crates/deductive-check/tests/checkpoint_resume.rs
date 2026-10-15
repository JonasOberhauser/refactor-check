//! Checkpoint integration: run the deductive-check machine under the
//! servyi-states checkpoint protocol, snapshot it, restore into a fresh
//! piece manager, and prove the resumed run skips recorded pieces
//! (zero solver calls beyond the preflight). Harness from aggregation.rs.

#![allow(clippy::unwrap_used, clippy::panic, unused_results)]

use anyhow::Result;
use async_trait::async_trait;

use deductive_check::checkpoint::{restore_snapshot, run_checkpointed};
use deductive_check::piece_manager::{DefaultDeductivePieceManager, MachineCheckpointable};
use deductive_check::provider::{
    AgentRequest, AgentResponse, CalledFunctionCode, FileSystemRequest, FileSystemResponse,
    FunctionInfo, GitRequest, GitResponse, Providers, PythonRequest, PythonResponse,
    RustAnalyzerRequest, RustAnalyzerResponse,
};
use deductive_check::code_piece::FunctionId;
use refactor_check_core::provider::{IOProvider, LlmRequest, LlmRole, SolverRequest, WithContext};
use refactor_check_core::smt::{SolverOutcome, SolverResult};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

const SPLITTER_OUT: &str =
    "```rust\nfn piece() {\n    let x = 1;\n    assert!(x > 0);\n    return;\n}\n```\n";
const SMT_OUT: &str =
    "```smt2\n(set-logic ALL)\n(declare-const x Int)\n(assert (> x 0))\n(check-sat)\n```\n";

struct MockLlm {
    formalizer_out: String,
}

#[async_trait]
impl IOProvider<LlmRequest, WithContext<String>> for MockLlm {
    async fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let resp = match input.role {
            LlmRole::Splitter => SPLITTER_OUT.to_string(),
            LlmRole::Formalizer | LlmRole::Fixer => self.formalizer_out.clone(),
            LlmRole::Judge => "REASONABLE".to_string(),
            LlmRole::SplittingJudge | LlmRole::Analyzer => String::new(),
        };
        Ok(WithContext { value: resp, context_id: input.context_id })
    }
}


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


struct CountingSolver {
    invokes: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl IOProvider<SolverRequest, WithContext<SolverResult>> for CountingSolver {
    async fn invoke(&self, _input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let _n = self.invokes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(WithContext {
            value: SolverResult {
                outcome: SolverOutcome::Unsat,
                stdout: "unsat".into(),
                stderr: String::new(),
            },
            context_id: _input.context_id,
        })
    }
}


#[test]
fn checkpointed_run_snapshots_and_resume_skips_recorded_pieces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("proj");
    std::fs::create_dir_all(project.join(".git")).expect("git dir");

    let snap_path = dir.path().join("cp.json");

    // ── Fresh run under the checkpoint protocol. ──
    let llm = MockLlm { formalizer_out: SMT_OUT.to_string() };
    let solver = CountingSolver { invokes: std::sync::atomic::AtomicUsize::new(0) };
    let mocks = Providers {
        llm: &llm,
        solver: &solver,
        rust_analyzer: &MockRa,
        git: &MockGit,
        filesystem: &MockFs,
        python: &MockPython,
        agent: &MockAgent { responses: Mutex::new(VecDeque::new()) },
    };
    let pm = DefaultDeductivePieceManager::default();
    let result = run_checkpointed(
        project.to_str().expect("utf8"),
        &mocks,
        &pm,
        &snap_path,
        std::time::Duration::from_millis(2),
    )
    .expect("checkpointed run");

    // One piece, closed by the Unsat solver.
    assert_eq!(result.closed_pieces.len(), 2, "fresh run closes both pieces");
    // Preflight (1) + piece check (1) = 2 solver invokes.
    let fresh_solver = solver.invokes.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(fresh_solver, 3, "preflight + 2 piece checks");

    // The snapshot exists and carries the outcome + result.
    let snap = restore_snapshot(&snap_path).expect("snapshot parses");
    assert_eq!(snap.outcomes.len(), 2, "both piece outcomes recorded");
    assert!(snap.result.is_some(), "final result in the shape");

    // ── Resumed run: fresh pm restored from the snapshot; the piece is
    // skipped — formalizer/solver for pieces must NOT run again. ──
    let llm2 = MockLlm { formalizer_out: SMT_OUT.to_string() };
    let solver2 = CountingSolver { invokes: std::sync::atomic::AtomicUsize::new(0) };
    let mocks2 = Providers {
        llm: &llm2,
        solver: &solver2,
        rust_analyzer: &MockRa,
        git: &MockGit,
        filesystem: &MockFs,
        python: &MockPython,
        agent: &MockAgent { responses: Mutex::new(VecDeque::new()) },
    };
    let mut pm2 = DefaultDeductivePieceManager::default();
    pm2.restore(&snap).expect("restore");
    let resumed = run_checkpointed(
        project.to_str().expect("utf8"),
        &mocks2,
        &pm2,
        dir.path().join("cp2.json"),
        std::time::Duration::from_millis(2),
    )
    .expect("resumed run");

    assert_eq!(
        resumed.closed_pieces.len(), 2,
        "resumed run reports the same closed pieces"
    );
    // ONLY the preflight solver call — the piece was skipped.
    let resumed_solver = solver2.invokes.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        resumed_solver, 1,
        "resume must skip the piece's solver call (preflight only)"
    );
}

