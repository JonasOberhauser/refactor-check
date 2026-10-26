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
/// The judge verdict that accepts a response.
const VERDICT_REASONABLE: &str = "REASONABLE";
/// Fake solver stdout values.
const STDOUT_UNSAT: &str = "unsat";
const STDOUT_SAT: &str = "sat";
const STDOUT_UNKNOWN: &str = "unknown";
/// Functions the mock RA lists.
const FIRST_FN_NAME: &str = "f0";
const SECOND_FN_NAME: &str = "f1";
/// Line span every mock function claims.
const MOCK_DEF_LINE: u32 = 1;
const MOCK_START_LINE: u32 = 1;
const MOCK_END_LINE: u32 = 5;
/// Fallback paths and refs the mock git reports.
const LIB_RS_PATH: &str = "src/lib.rs";
const WALK_OUTPUT: &str = "src/lib.rs\n";
const COMMIT_HASH: &str = "deadbeefcafebabe1234567890abcdef12345678";
/// Minimal SMT-LIB the mock python provider falls back to.
const MINIMAL_SMT2: &str = "(check-sat)";
/// Stub file content the mock RA returns.
const STUB_MAIN_SRC: &str = "fn main() {}";
/// Scripted agent responses.
const AGENT_OK: &str = "OK";
const AGENT_NO_BUG: &str = "No bug found.";
const BUG_REPORT_F0: &str = "Found a bug: x can be 0.";
const VERDICT_RETRY: &str = "RETRY";
const F0_BUG_MESSAGE: &str = "Bug in f0.";
const F1_BUG_MESSAGE: &str = "Bug in f1.";
const NO_FORMULA_OUT: &str = "I cannot produce a formula.";
const NO_FORMULA_BUG: &str = "Bug: no formula could be produced.";
/// Directory marker that makes the machine treat the tempdir as a repo.
const GIT_DIR: &str = ".git";
/// Expected counts and indexes into the results.
const EXPECTED_BUG_REPORTS: usize = 1;
const FIRST_BUG_REPORT: usize = 0;
const EXPECTED_TOTAL_PIECES: usize = 2;
const EXPECTED_TOTAL_FUNCTIONS: usize = 2;
const EXPECTED_BUG_COUNT: usize = 2;

// ── LLM: role-fixed responses (judge always REASONABLE) ────────────────────

struct MockLlm {
    formalizer_out: String,
}

impl IOProvider<LlmRequest, WithContext<String>> for MockLlm {
    fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let resp = match input.role {
            LlmRole::Splitter => SPLITTER_OUT.to_string(),
            LlmRole::Formalizer | LlmRole::Fixer => self.formalizer_out.clone(),
            LlmRole::Judge => VERDICT_REASONABLE.to_string(),
            LlmRole::SplittingJudge | LlmRole::Analyzer => String::new(),
        };
        Ok(WithContext { value: resp, context_id: input.context_id })
    }
}

// ── Solver: scripted outcomes (Sat in cycle 1, Unsat in cycle 2) ───────────

struct MockSolver {
    outcomes: Mutex<VecDeque<SolverOutcome>>,
}

impl IOProvider<SolverRequest, WithContext<SolverResult>> for MockSolver {
    fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let outcome = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(SolverOutcome::Unsat);
        let stdout = match &outcome {
            SolverOutcome::Unsat => STDOUT_UNSAT.to_string(),
            SolverOutcome::Sat => STDOUT_SAT.to_string(),
            SolverOutcome::Unknown => STDOUT_UNKNOWN.to_string(),
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

impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for MockRa {
    fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        Ok(match input {
            RustAnalyzerRequest::ListFunctions { files, .. } => {
                let file = files.first().cloned().unwrap_or_else(|| PathBuf::from(LIB_RS_PATH));
                RustAnalyzerResponse::FunctionList(vec![
                    function_info(file.clone(), FIRST_FN_NAME),
                    function_info(file, SECOND_FN_NAME),
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
                RustAnalyzerResponse::FileContent(STUB_MAIN_SRC.to_string())
            }
        })
    }
}

fn function_info(file: PathBuf, name: &str) -> FunctionInfo {
    FunctionInfo {
        id: FunctionId::new(file, name, None, MOCK_DEF_LINE),
        body: String::new(),
        start_line: MOCK_START_LINE,
        end_line: MOCK_END_LINE,
        docs: String::new(),
        has_guarantees: true,
    }
}

// ── Git / FS / Python: fixed happy-path responses ──────────────────────────

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
            GitRequest::FindChangedRustFiles { .. } => GitResponse { success: true, output: String::new() },
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
        let smtlib = if input.script.trim().is_empty() {
            MINIMAL_SMT2.to_string()
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

impl IOProvider<AgentRequest, AgentResponse> for MockAgent {
    fn invoke(&self, _input: AgentRequest) -> Result<AgentResponse> {
        let (success, stdout) = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((true, AGENT_NO_BUG.to_string()));
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

#[test]
fn bug_report_survives_restart_cycle() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(GIT_DIR)).expect("create .git marker");

    let mocks = Mocks {
        llm: MockLlm { formalizer_out: SMT_OUT.to_string() },
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
                (true, AGENT_OK.to_string()),               // preflight
                (true, BUG_REPORT_F0.to_string()),          // cycle 1 f0 -> bug report
                (true, VERDICT_RETRY.to_string()),          // cycle 1 f1 -> restart
            ])),
        },
    };

    let pm = DefaultDeductivePieceManager::default();
    let result = machine::run(
        project.path().to_str().expect("utf8 temp path"),
        &mocks.providers(),
        &pm,
    )
    
    .expect("machine ran");

    assert_eq!(
        result.bug_reports.len(),
        EXPECTED_BUG_REPORTS,
        "the f0 bug report from cycle 1 must survive the restart into the final result; \
         got bug_reports={:?} (closed={}, unverified={})",
        result.bug_reports,
        result.closed_pieces.len(),
        result.unverified_pieces.len(),
    );
    assert!(
        result.bug_reports[FIRST_BUG_REPORT].description.contains("x can be 0"),
        "preserved bug report should be f0's: {:?}",
        result.bug_reports[FIRST_BUG_REPORT]
    );
}

#[test]
fn totals_reported_when_terminating_via_problem_analyzer() {
    // When the machine terminates through ProblemAnalyzer's own Result path
    // (unverified pieces all reported as bugs, none RETRY), the final
    // VerificationResult must report the real totals. Previously
    // ProblemAnalyzer hardcoded total_functions: 0 / total_pieces: 0.
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(GIT_DIR)).expect("create .git marker");

    let mocks = Mocks {
        llm: MockLlm { formalizer_out: SMT_OUT.to_string() },
        solver: MockSolver {
            outcomes: Mutex::new(VecDeque::from([
                SolverOutcome::Unsat, // preflight (outcome ignored)
                SolverOutcome::Sat,   // f0 -> unverified
                SolverOutcome::Sat,   // f1 -> unverified
            ])),
        },
        ra: MockRa,
        git: MockGit,
        fs: MockFs,
        python: MockPython,
        agent: MockAgent {
            responses: Mutex::new(VecDeque::from([
                (true, AGENT_OK.to_string()),       // preflight
                (true, F0_BUG_MESSAGE.to_string()), // f0 -> bug report (no RETRY)
                (true, F1_BUG_MESSAGE.to_string()), // f1 -> bug report (no RETRY)
            ])),
        },
    };

    let pm = DefaultDeductivePieceManager::default();
    let result = machine::run(
        project.path().to_str().expect("utf8 temp path"),
        &mocks.providers(),
        &pm,
    )
    
    .expect("machine ran");

    assert_eq!(
        result.total_pieces, EXPECTED_TOTAL_PIECES,
        "total_pieces must reflect the pieces processed (2), not 0"
    );
    assert_eq!(
        result.total_functions, EXPECTED_TOTAL_FUNCTIONS,
        "total_functions must reflect the functions verified (2), not 0"
    );
    assert_eq!(result.bug_reports.len(), EXPECTED_BUG_COUNT, "both pieces should be bug reports");
}

#[test]
fn no_formula_on_last_judge_attempt_does_not_panic() {
    // When the formalizer yields 0 parseable formulas, process_piece's judge
    // loop exhausts all attempts via the "no formulas extracted" branch,
    // leaving the piece in GetContext. The post-loop transition must not
    // assume the piece is in Judge (it used to -> phase-tracker panic).
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(GIT_DIR)).expect("create .git marker");

    let mocks = Mocks {
        llm: MockLlm { formalizer_out: NO_FORMULA_OUT.to_string() },
        solver: MockSolver {
            outcomes: Mutex::new(VecDeque::from([
                SolverOutcome::Unsat, // preflight only (0 formulas -> no solver checks)
            ])),
        },
        ra: MockRa,
        git: MockGit,
        fs: MockFs,
        python: MockPython,
        agent: MockAgent {
            responses: Mutex::new(VecDeque::from([
                (true, AGENT_OK.to_string()),                   // preflight
                (true, NO_FORMULA_BUG.to_string()),             // ProblemAnalyzer
            ])),
        },
    };

    let pm = DefaultDeductivePieceManager::default();
    // Before the fix this panics inside process_piece; after, the piece
    // surfaces as unverified/bug.
    let result = machine::run(
        project.path().to_str().expect("utf8 temp path"),
        &mocks.providers(),
        &pm,
    )
    
    .expect("machine must not panic; piece should surface as bug/unverified");

    assert!(
        !result.bug_reports.is_empty() || !result.unverified_pieces.is_empty(),
        "the no-formula piece should surface as a bug or unverified, not crash"
    );
}
