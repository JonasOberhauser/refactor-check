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
/// The judge verdict that accepts a response.
const VERDICT_REASONABLE: &str = "REASONABLE";
/// Separator used when flattening a fixer prompt for inspection.
const PROMPT_JOIN_SEP: &str = "\n---\n";
/// Marker identifying the formalizer's broken pyz3 script.
const BROKEN_SCRIPT_MARKER: &str = "undeclared_name_typo";
/// Fake solver stdout.
const STDOUT_UNSAT: &str = "unsat";
/// The single mock function and its line span.
const FIRST_FN_NAME: &str = "f0";
const MOCK_DEF_LINE: u32 = 1;
const MOCK_START_LINE: u32 = 1;
const MOCK_END_LINE: u32 = 5;
/// Fallback paths and refs the mock git reports.
const LIB_RS_PATH: &str = "src/lib.rs";
const WALK_OUTPUT: &str = "src/lib.rs\n";
const COMMIT_HASH: &str = "deadbeefcafebabe1234567890abcdef12345678";
/// Stub file content the mock RA returns.
const STUB_MAIN_SRC: &str = "fn main() {}";
/// Agent's happy-path response.
const AGENT_NO_BUG: &str = "No bug found.";
/// Directory marker that makes the machine treat the tempdir as a repo.
const GIT_DIR: &str = ".git";
/// Expected counts and indexes into the recorded prompts/results.
const EXPECTED_FIXER_CALLS: usize = 1;
const FIRST_FIXER_PROMPT: usize = 0;
const EXPECTED_CLOSED_PIECES: usize = 1;

// ── LLM: records fixer prompts ──────────────────────────────────────────────

struct MockLlm {
    fixer_prompts: Mutex<Vec<String>>,
}

impl IOProvider<LlmRequest, WithContext<String>> for MockLlm {
    fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
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
                    .join(PROMPT_JOIN_SEP);
                self.fixer_prompts.lock().expect("fixer log lock").push(prompt);
                FIXED_SMT_OUT.to_string()
            }
            LlmRole::Judge => VERDICT_REASONABLE.to_string(),
            LlmRole::SplittingJudge | LlmRole::Analyzer => String::new(),
        };
        Ok(WithContext { value: resp, context_id: input.context_id })
    }
}

// ── Python: fails exactly once, exactly like a nonzero exit ─────────────────

struct FailThenEchoPython;

impl IOProvider<PythonRequest, PythonResponse> for FailThenEchoPython {
    fn invoke(&self, input: PythonRequest) -> Result<PythonResponse> {
        // The preflight probe (`print("(check-sat)")`) must succeed; the
        // formalizer's broken pyz3 script fails exactly like a nonzero
        // python3 exit (the branch's bail; main parks the gate instead).
        if input.script.contains(BROKEN_SCRIPT_MARKER) {
            return Err(anyhow!(
                "python3 failed (exit Some(1)): NameError: name 'undeclared_name_typo' is not defined"
            ));
        }
        Ok(PythonResponse { smtlib: input.script, explanation: String::new() })
    }
}

// ── Solver / RA / Git / FS / Agent: happy-path ─────────────────────────────

struct MockSolver;
impl IOProvider<SolverRequest, WithContext<SolverResult>> for MockSolver {
    fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let _ = input;
        Ok(WithContext {
            value: SolverResult {
                outcome: SolverOutcome::Unsat,
                stdout: STDOUT_UNSAT.to_string(),
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
                let file = files.first().cloned().unwrap_or_else(|| PathBuf::from(LIB_RS_PATH));
                RustAnalyzerResponse::FunctionList(vec![FunctionInfo {
                    id: FunctionId::new(file, FIRST_FN_NAME, None, MOCK_DEF_LINE),
                    body: String::new(),
                    start_line: MOCK_START_LINE,
                    end_line: MOCK_END_LINE,
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
                RustAnalyzerResponse::FileContent(STUB_MAIN_SRC.to_string())
            }
        })
    }
}

struct MockGit;
impl IOProvider<GitRequest, GitResponse> for MockGit {
    fn invoke(&self, input: GitRequest) -> Result<GitResponse> {
        Ok(match input {
            GitRequest::WalkRustFiles { .. } => {
                GitResponse { success: true, output: WALK_OUTPUT.to_string() }
            }
            GitRequest::CurrentCommitHash => {
                GitResponse { success: true, output: COMMIT_HASH.to_string() }
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

struct MockAgent;
impl IOProvider<AgentRequest, AgentResponse> for MockAgent {
    fn invoke(&self, input: AgentRequest) -> Result<AgentResponse> {
        let _ = input;
        Ok(AgentResponse { success: true, stdout: AGENT_NO_BUG.to_string() })
    }
}

// ── The test ────────────────────────────────────────────────────────────────

#[test]
fn python_script_failure_goes_to_the_fixer_not_the_unknown_bucket() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(GIT_DIR)).expect("create .git marker");

    let llm: &'static MockLlm = Box::leak(Box::new(MockLlm { fixer_prompts: Mutex::new(Vec::new()) }));
    let mocks = Providers {
        llm,
        solver: &MockSolver,
        rust_analyzer: &MockRa,
        git: &MockGit,
        filesystem: &MockFs,
        python: &FailThenEchoPython,
        agent: &MockAgent,
    };

    let providers: &'static Providers<'static> = Box::leak(Box::new(mocks));
    let pm: &'static DefaultDeductivePieceManager =
        Box::leak(Box::new(DefaultDeductivePieceManager::default()));
    let result = machine::run(
        project.path().to_str().expect("utf8 temp path"),
        providers,
        pm,
    )
    
    .expect("machine ran");

    let fixer_prompts = llm.fixer_prompts.lock().expect("fixer log lock");
    assert_eq!(
        fixer_prompts.len(),
        EXPECTED_FIXER_CALLS,
        "the failed pyz3 script must be routed to the fixer LLM exactly once; \
         prompts seen: {fixer_prompts:?} (closed={}, unverified={})",
        result.closed_pieces.len(),
        result.unverified_pieces.len(),
    );
    assert!(
        fixer_prompts[FIRST_FIXER_PROMPT].contains("NameError: name 'undeclared_name_typo'"),
        "the fixer prompt must carry the python stderr so the LLM can actually \
         fix the script; got: {}",
        fixer_prompts[FIRST_FIXER_PROMPT]
    );
    assert_eq!(
        result.closed_pieces.len(),
        EXPECTED_CLOSED_PIECES,
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
