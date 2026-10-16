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
//!
//! Also: the fan-outs run concurrently, pieces still aggregate in piece order,
//! and each piece commits only its own formula files.
#![allow(clippy::unwrap_used, clippy::panic, unused_results)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::Result;

use deductive_check::code_piece::FunctionId;
use deductive_check::machine;
use deductive_check::piece_manager::DefaultDeductivePieceManager;
use deductive_check::provider::{
    AgentRequest, AgentResponse, CalledFunction, CalledFunctionCode, FileSystemRequest, FileSystemResponse,
    FunctionInfo, GitRequest, GitResponse, Providers, PythonRequest, PythonResponse,
    RustAnalyzerRequest, RustAnalyzerResponse,
};
use refactor_check_core::provider::{IOProvider, LlmRequest, LlmRole, SolverRequest, WithContext};
use refactor_check_core::smt::{SolverOutcome, SolverResult};

const SPLITTER_OUT: &str =
    "```rust\nfn piece() {\n    let x = 1;\n    assert!(x > 0);\n    return;\n}\n```\n";
const SMT_OUT: &str = "```smt2\n(set-logic ALL)\n(declare-const x Int)\n(assert (> x 0))\n(check-sat)\n```\n";

// ── LLM: role-fixed responses (judge always REASONABLE) ────────────────────

struct MockLlm {
    formalizer_out: String,
}

impl IOProvider<LlmRequest, WithContext<String>> for MockLlm {
    fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        let resp = match input.role {
            LlmRole::Splitter => SPLITTER_OUT.to_string(),
            LlmRole::Formalizer | LlmRole::Fixer => self.formalizer_out.clone(),
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

impl IOProvider<SolverRequest, WithContext<SolverResult>> for MockSolver {
    fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
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

impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for MockRa {
    fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
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
impl IOProvider<GitRequest, GitResponse> for MockGit {
    fn invoke(&self, input: GitRequest) -> Result<GitResponse> {
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
impl IOProvider<FileSystemRequest, FileSystemResponse> for MockFs {
    fn invoke(&self, input: FileSystemRequest) -> Result<FileSystemResponse> {
        Ok(FileSystemResponse { path: input.dir.join(&input.filename) })
    }
}

struct MockPython;
impl IOProvider<PythonRequest, PythonResponse> for MockPython {
    fn invoke(&self, input: PythonRequest) -> Result<PythonResponse> {
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
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git marker");

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

#[test]
fn totals_reported_when_terminating_via_problem_analyzer() {
    // When the machine terminates through ProblemAnalyzer's own Result path
    // (unverified pieces all reported as bugs, none RETRY), the final
    // VerificationResult must report the real totals. Previously
    // ProblemAnalyzer hardcoded total_functions: 0 / total_pieces: 0.
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git marker");

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
                (true, "OK".to_string()),             // preflight
                (true, "Bug in f0.".to_string()),     // f0 -> bug report (no RETRY)
                (true, "Bug in f1.".to_string()),     // f1 -> bug report (no RETRY)
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
        result.total_pieces, 2,
        "total_pieces must reflect the pieces processed (2), not 0"
    );
    assert_eq!(
        result.total_functions, 2,
        "total_functions must reflect the functions verified (2), not 0"
    );
    assert_eq!(result.bug_reports.len(), 2, "both pieces should be bug reports");
}

#[test]
fn no_formula_on_last_judge_attempt_does_not_panic() {
    // When the formalizer yields 0 parseable formulas, process_piece's judge
    // loop exhausts all attempts via the "no formulas extracted" branch,
    // leaving the piece in GetContext. The post-loop transition must not
    // assume the piece is in Judge (it used to -> phase-tracker panic).
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git marker");

    let mocks = Mocks {
        llm: MockLlm { formalizer_out: "I cannot produce a formula.".to_string() },
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
                (true, "OK".to_string()),                                  // preflight
                (true, "Bug: no formula could be produced.".to_string()),  // ProblemAnalyzer
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

// ── Concurrency: rendezvous wrappers, recording git ────────────────────────

/// Holds callers until `expected` of them are in flight together.
struct Rendezvous {
    expected: usize,
    arrived: Mutex<usize>,
    all_arrived: Condvar,
}

const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(10);

impl Rendezvous {
    fn new(expected: usize) -> Self {
        Self { expected, arrived: Mutex::new(0), all_arrived: Condvar::new() }
    }

    fn wait(&self, what: &str) {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        self.all_arrived.notify_all();
        let (arrived, wait) = self
            .all_arrived
            .wait_timeout_while(arrived, RENDEZVOUS_TIMEOUT, |n| *n < self.expected)
            .unwrap();
        assert!(!wait.timed_out(), "only {} of {} {what} calls were in flight together", *arrived, self.expected);
    }
}

/// [`MockLlm`] whose splitter calls meet.
struct RendezvousLlm {
    inner: MockLlm,
    splits: Rendezvous,
}

impl IOProvider<LlmRequest, WithContext<String>> for RendezvousLlm {
    fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        if input.role == LlmRole::Splitter {
            self.splits.wait("splitter");
        }
        self.inner.invoke(input)
    }
}

/// [`MockRa`] where every function calls two helpers; the function-code and
/// the helper-code lookups each meet.
struct RendezvousRa {
    code: Rendezvous,
    callee_code: Rendezvous,
}

impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for RendezvousRa {
    fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        match input {
            RustAnalyzerRequest::GetFunctionCode { .. } => self.code.wait("function code"),
            RustAnalyzerRequest::GetCalledFunctions { .. } => {
                let called = ["helper_a", "helper_b"]
                    .iter()
                    .map(|name| CalledFunction { name: name.to_string(), file: None, start_line: None })
                    .collect();
                return Ok(RustAnalyzerResponse::CalledFunctionList(called));
            }
            RustAnalyzerRequest::GetCalledFunctionCode { .. } => self.callee_code.wait("helper code"),
            _ => {}
        }
        MockRa.invoke(input)
    }
}

/// Unsat for everything; the formula checks (not the preflight) meet.
struct RendezvousSolver(Rendezvous);

impl IOProvider<SolverRequest, WithContext<SolverResult>> for RendezvousSolver {
    fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        if input.formula.contains("declare-const") {
            self.0.wait("solver");
        }
        Ok(WithContext {
            value: SolverResult { outcome: SolverOutcome::Unsat, stdout: "unsat".to_string(), stderr: String::new() },
            context_id: input.context_id,
        })
    }
}

/// Records what each commit staged.
#[derive(Default)]
struct RecordingGit {
    staged: Mutex<Vec<PathBuf>>,
    commits: Mutex<Vec<(String, Vec<PathBuf>)>>,
}

impl IOProvider<GitRequest, GitResponse> for RecordingGit {
    fn invoke(&self, input: GitRequest) -> Result<GitResponse> {
        match input {
            GitRequest::AddFiles { paths } => self.staged.lock().unwrap().extend(paths),
            GitRequest::Commit { message } => {
                let staged = std::mem::take(&mut *self.staged.lock().unwrap());
                self.commits.lock().unwrap().push((message, staged));
            }
            other => return MockGit.invoke(other),
        }
        Ok(GitResponse { success: true, output: String::new() })
    }
}

/// Two functions, one piece each, two helpers and two formulas per piece: the
/// code lookups, the splits, all four helper lookups and all four formula
/// checks run concurrently.
#[test]
fn parallel_pieces_aggregate_in_order_and_commit_separately() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git marker");

    let llm = RendezvousLlm {
        inner: MockLlm { formalizer_out: format!("{SMT_OUT}{SMT_OUT}") },
        splits: Rendezvous::new(2),
    };
    let ra = RendezvousRa { code: Rendezvous::new(2), callee_code: Rendezvous::new(4) };
    let solver = RendezvousSolver(Rendezvous::new(4));
    let git = RecordingGit::default();
    let agent = MockAgent { responses: Mutex::new(VecDeque::from([(true, "OK".to_string())])) };
    let providers = Providers {
        llm: &llm,
        solver: &solver,
        rust_analyzer: &ra,
        git: &git,
        filesystem: &MockFs,
        python: &MockPython,
        agent: &agent,
    };

    let pm = DefaultDeductivePieceManager::default();
    let result = machine::run(project.path().to_str().expect("utf8 temp path"), &providers, &pm)
        .expect("machine ran");

    let names: Vec<&str> = result.closed_pieces.iter().map(|p| p.function_id.name.as_str()).collect();
    assert_eq!(names, ["f0", "f1"], "pieces aggregate in piece order");

    let commits = git.commits.lock().unwrap();
    assert_eq!(commits.len(), 2, "one commit per piece: {commits:?}");
    for (message, staged) in commits.iter() {
        let ctx = message
            .strip_prefix("Add formulas for ctx ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_else(|| panic!("unexpected commit message {message:?}"));
        assert_eq!(staged.len(), 2, "commit {message:?} must stage its piece's two formulas");
        for path in staged {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap();
            assert!(name.starts_with(&format!("ctx{ctx}.")), "commit {message:?} staged {name}");
        }
    }
}
