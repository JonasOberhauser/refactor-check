//! Proves the python-failure routing fix: a pyz3 script that fails must be
//! fed to the fixer LLM (with the python stderr), not closed as Unknown and
//! not parked on the human error gate.
//!
//! Scenario, deterministic providers, ONE function/piece:
//!   formalizer -> broken pyz3 script (```python block)
//!   python     -> first invoke fails with the real provider's error shape
//!                 ("python3 failed (exit Some(1)): NameError: boom")
//!   fixer      -> returns a plain smt2 formula (no python needed)
//!   solver     -> Unsat for the fixed formula
//!
//! Expected: the fixer is consulted exactly once, its prompt carries the
//! python stderr, and the piece closes (no unverified/Unknown left behind).

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
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
const BROKEN_PYZ3_OUT: &str =
    "```python\nfrom z3 import *\nx = Int('x')\nprove(undeclared_name_typo)\n```\n";
const FIXED_SMT_OUT: &str =
    "```smt2\n(set-logic ALL)\n(declare-const x Int)\n(assert (> x 0))\n(check-sat)\n```\n";

// ── LLM: records fixer prompts ──────────────────────────────────────────────

struct MockLlm {
    fixer_prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl IOProvider<LlmRequest, WithContext<String>> for MockLlm {
    async fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let resp = match input.role {
            LlmRole::Splitter => SPLITTER_OUT.to_string(),
            LlmRole::Formalizer => BROKEN_PYZ3_OUT.to_string(),
            LlmRole::Fixer => {
                let prompt = input
                    .messages
                    .iter()
                    .rev()
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                self.fixer_prompts.lock().expect("fixer log lock").push(prompt);
                FIXED_SMT_OUT.to_string()
            }
            LlmRole::Judge => "REASONABLE".to_string(),
            LlmRole::SplittingJudge | LlmRole::Analyzer => String::new(),
        };
        Ok(WithContext { value: resp, context_id: input.context_id })
    }
}

// ── Python: fails exactly once, exactly like a nonzero exit ─────────────────

struct FailThenEchoPython;

#[async_trait]
impl IOProvider<PythonRequest, PythonResponse> for FailThenEchoPython {
    async fn invoke(&self, input: PythonRequest) -> Result<PythonResponse> {
        // The preflight probe (`print("(check-sat)")`) must succeed; the
        // formalizer's broken pyz3 script fails exactly like a nonzero
        // python3 exit (the branch's bail; main parks the gate instead).
        if input.script.contains("undeclared_name_typo") {
            return Err(anyhow!(
                "python3 failed (exit Some(1)): NameError: name 'undeclared_name_typo' is not defined"
            ));
        }
        Ok(PythonResponse { smtlib: input.script, explanation: String::new() })
    }
}

// ── Solver / RA / Git / FS / Agent: happy-path ─────────────────────────────

struct MockSolver;
#[async_trait]
impl IOProvider<SolverRequest, WithContext<SolverResult>> for MockSolver {
    async fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let _ = input;
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
#[async_trait]
impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for MockRa {
    async fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        Ok(match input {
            RustAnalyzerRequest::ListFunctions { files, .. } => {
                let file = files.first().cloned().unwrap_or_else(|| PathBuf::from("src/lib.rs"));
                RustAnalyzerResponse::FunctionList(vec![FunctionInfo {
                    id: FunctionId::new(file, "f0", None, 1),
                    body: String::new(),
                    start_line: 1,
                    end_line: 5,
                    docs: String::new(),
                    has_guarantees: true,
                }])
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

struct MockAgent;
#[async_trait]
impl IOProvider<AgentRequest, AgentResponse> for MockAgent {
    async fn invoke(&self, input: AgentRequest) -> Result<AgentResponse> {
        let _ = input;
        Ok(AgentResponse { success: true, stdout: "No bug found.".to_string() })
    }
}

// ── The test ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn python_script_failure_goes_to_the_fixer_not_the_unknown_bucket() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git marker");

    let llm = MockLlm { fixer_prompts: Mutex::new(Vec::new()) };
    let mocks = Providers {
        llm: &llm,
        solver: &MockSolver,
        rust_analyzer: &MockRa,
        git: &MockGit,
        filesystem: &MockFs,
        python: &FailThenEchoPython,
        agent: &MockAgent,
    };

    let pm = DefaultDeductivePieceManager::default();
    let result = machine::run(
        project.path().to_str().expect("utf8 temp path"),
        &mocks,
        &pm,
    )
    .await
    .expect("machine ran");

    let fixer_prompts = llm.fixer_prompts.lock().expect("fixer log lock");
    assert_eq!(
        fixer_prompts.len(),
        1,
        "the failed pyz3 script must be routed to the fixer LLM exactly once; \
         prompts seen: {fixer_prompts:?} (closed={}, unverified={})",
        result.closed_pieces.len(),
        result.unverified_pieces.len(),
    );
    assert!(
        fixer_prompts[0].contains("NameError: name 'undeclared_name_typo'"),
        "the fixer prompt must carry the python stderr so the LLM can actually \
         fix the script; got: {}",
        fixer_prompts[0]
    );
    assert_eq!(
        result.closed_pieces.len(),
        1,
        "after the fix, the fixed formula must be checked and close the piece \
         (closed={}, unverified={})",
        result.closed_pieces.len(),
        result.unverified_pieces.len(),
    );
    assert!(
        result.unverified_pieces.is_empty(),
        "a python failure is LLM-recoverable and must not leave the piece \
         unverified/Unknown"
    );
}
