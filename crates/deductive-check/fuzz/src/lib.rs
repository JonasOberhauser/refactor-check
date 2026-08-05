//! Fuzz harness for the deductive-check state machine.
//!
//! All providers share a single [`FuzzData`] byte stream (via `Arc<Mutex<...>>`),
//! so one libFuzzer input deterministically drives every provider call in
//! sequence. The two context-bearing providers (LLM, solver) use
//! [`ContextFuzzProvider`], which moves the non-cloneable `Box<ContextId>` out
//! of the owned request after the fuzz draw borrows it.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use arbitrary::Unstructured;
use async_trait::async_trait;
use tracing::info;

use deductive_check::consts;
use deductive_check::provider::{
    AgentRequest, AgentResponse, CalledFunction, CalledFunctionCode, FileSystemRequest,
    FileSystemResponse, FunctionInfo, GitRequest, GitResponse, Providers, PythonRequest,
    PythonResponse, RustAnalyzerRequest, RustAnalyzerResponse,
};
use deductive_check::{code_piece::FunctionId, machine, piece_manager::DefaultDeductivePieceManager};
use refactor_check_core::context_id::ContextId;
use refactor_check_core::provider::{LlmRequest, LlmRole, SolverRequest, WithContext};
use refactor_check_core::smt::{SolverOutcome, SolverResult};
use servyi_ioprovider::{fuzz_stream_from_bytes, Fuzz, FuzzData, FuzzProvider, IOProvider};

// ---------------------------------------------------------------------------
// Context pass-through adapter (for LLM + solver, whose responses wrap `WithContext`)
// ---------------------------------------------------------------------------

/// Types whose ownership includes a `Box<ContextId>` that must be threaded
/// through to the response. Implemented for the two request types whose
/// responses are `WithContext<_>`.
pub trait ExtractContext {
    fn take_context_id(self) -> Box<ContextId>;
}

impl ExtractContext for LlmRequest {
    fn take_context_id(self) -> Box<ContextId> {
        self.context_id
    }
}

impl ExtractContext for SolverRequest {
    fn take_context_id(self) -> Box<ContextId> {
        self.context_id
    }
}

/// Implements `IOProvider<I, WithContext<T>>` by drawing `T` from an inner
/// [`Fuzz<I, T>`] impl and re-attaching the request's `ContextId`.
///
/// The request is owned in `invoke`: it is borrowed immutably during the fuzz
/// draw, then — once the borrow ends — its `Box<ContextId>` is moved out. This
/// preserves the project's no-clone `ContextId` invariant.
pub struct ContextFuzzProvider<F> {
    fuzz: F,
    stream: Arc<Mutex<FuzzData>>,
}

impl<F> ContextFuzzProvider<F> {
    pub fn new(fuzz: F, stream: Arc<Mutex<FuzzData>>) -> Self {
        Self { fuzz, stream }
    }
}

#[async_trait]
impl<F, I, T> IOProvider<I, WithContext<T>> for ContextFuzzProvider<F>
where
    F: Fuzz<I, T>,
    I: ExtractContext + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    async fn invoke(&self, input: I) -> Result<WithContext<T>> {
        let value = {
            let mut guard = self.stream.lock().expect("fuzz stream poisoned");
            // Occasionally inject a synthetic provider error, modelling the
            // network/rate-limit failures the real LLM client and the solver
            // subprocess can raise. Rare and suppressed on an exhausted stream
            // so deep state-machine paths are still reached on most runs and
            // exhausted streams still terminate cleanly.
            if let Some(err) = maybe_fail(&mut guard) {
                return Err(err);
            }
            guard.draw(|u| self.fuzz.fuzz(&input, u))
        };
        let context_id = input.take_context_id();
        Ok(WithContext { value, context_id })
    }
}

/// Draw a byte and, ~0.4% of the time (only on a non-exhausted stream, since
/// `int_in_range` returns `Ok(start)` = 0 when empty), return a synthetic
/// provider error. This exercises the state machine's provider-error paths
/// (which either propagate to run termination or demote a piece to
/// "unverified: Processing error").
fn maybe_fail(guard: &mut FuzzData) -> Option<anyhow::Error> {
    let b: u8 = guard.draw(|u| u.int_in_range(0u8..=255).unwrap_or(0));
    (b == 255).then(|| anyhow::anyhow!("synthetic provider failure (fuzz-injected)"))
}

// ---------------------------------------------------------------------------
// Fuzz impls
// ---------------------------------------------------------------------------

/// Truncate + collapse newlines for readable trace output.
fn truncate(s: &str, max: usize) -> String {
    let collapsed = s.replace('\n', "\\n").replace('\r', "");
    if collapsed.chars().count() > max {
        format!("{}…", collapsed.chars().take(max).collect::<String>())
    } else {
        collapsed
    }
}

// ---- LLM -----------------------------------------------------------------

pub struct FuzzLlm;

impl Fuzz<LlmRequest, String> for FuzzLlm {
    fn fuzz(&self, input: &LlmRequest, u: &mut Unstructured) -> String {
        let result = match input.role {
            LlmRole::Splitter => fuzz_splitter(u),
            LlmRole::Formalizer => fuzz_formalizer(u),
            LlmRole::Fixer => fuzz_smt_blocks(u, 1, 1),
            LlmRole::Judge => fuzz_judge(u),
            LlmRole::SplittingJudge | LlmRole::Analyzer => fuzz_text(u),
        };
        let ctx = &*input.context_id;
        let last_user = input
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, deductive_check::llm::Role::User))
            .map(|m| m.content.as_str())
            .unwrap_or("");
        info!(
            %ctx,
            role = ?input.role,
            sent = %truncate(last_user, 160),
            recv = %truncate(&result, 300),
            "fuzz llm",
        );
        result
    }
}

/// Splitter: 0-2 fenced code blocks (each non-empty, so `type_invariant` holds).
fn fuzz_splitter(u: &mut Unstructured) -> String {
    let n = u.int_in_range(0..=2usize).unwrap_or(1);
    let mut out = String::new();
    for i in 0..n {
        let line = u.int_in_range(1..=50u32).unwrap_or(1);
        let val = u.int_in_range(0..=99i32).unwrap_or(0);
        out.push_str(&format!(
            "```rust\n// Start point: src/lib.rs:{line}\nfn piece_{i}() {{\n    let x = {val};\n    return;\n}}\n```\n\n"
        ));
    }
    out
}

/// Formalizer: usually fenced ```smt2 blocks, but ~10% of the time (only on a
/// non-exhausted stream) emit plain prose with no parseable formula. That
/// exercises process_piece's "no formulas extracted" branch, which appends an
/// elaboration and retries up to MAX_JUDGE_ATTEMPTS. Range-start (0, the
/// exhausted value) maps to a normal formula so exhausted streams still close.
fn fuzz_formalizer(u: &mut Unstructured) -> String {
    if u.int_in_range(0u8..=9).unwrap_or(0) == 9 {
        return "I cannot produce a formula for this code piece.".to_string();
    }
    fuzz_smt_blocks(u, 1, 2)
}

/// Formalizer/Fixer: `count` fenced ```smt2 blocks so `extract_formulas_from_response` picks them up.
fn fuzz_smt_blocks(u: &mut Unstructured, lo: usize, hi: usize) -> String {
    let n = u.int_in_range(lo..=hi).unwrap_or(lo);
    let mut out = String::new();
    for _ in 0..n {
        let assert = match u.int_in_range(0..=2u8).unwrap_or(0) {
            0 => "(> x 0)",
            1 => "(< x 100)",
            _ => "(= x 0)",
        };
        out.push_str(&format!(
            "```smt2\n(set-logic ALL)\n(declare-const x Int)\n(assert {assert})\n(check-sat)\n```\n\n"
        ));
    }
    out
}

/// Judge: either the exact token (piece closes/verifies) or unrelated feedback
/// (loops, bounded by `MAX_JUDGE_ATTEMPTS`). The range start (`0`) maps to
/// REASONABLE, so an *exhausted* stream terminates pieces rather than looping
/// forever (`int_in_range` returns `Ok(start)` on an empty `Unstructured`).
fn fuzz_judge(u: &mut Unstructured) -> String {
    if u.int_in_range(0u8..=9).unwrap_or(0) < 7 {
        consts::JUDGE_REASONABLE.to_string()
    } else {
        let n = u.int_in_range(1..=80usize).unwrap_or(1);
        let bytes = u.bytes(n).unwrap_or(&[]);
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn fuzz_text(u: &mut Unstructured) -> String {
    let n = u.int_in_range(0..=64usize).unwrap_or(16);
    let bytes = u.bytes(n).unwrap_or(&[]);
    String::from_utf8_lossy(bytes).into_owned()
}

// ---- Solver --------------------------------------------------------------

pub struct FuzzSolver;

impl Fuzz<SolverRequest, SolverResult> for FuzzSolver {
    fn fuzz(&self, input: &SolverRequest, u: &mut Unstructured) -> SolverResult {
        let outcome = match u.int_in_range(0u8..=9).unwrap_or(4) {
            0..=4 => SolverOutcome::Unsat,
            5..=7 => SolverOutcome::Sat,
            8 => SolverOutcome::Unknown,
            _ => SolverOutcome::Error("parse error\n(line 1)".to_string()),
        };
        let stdout = match &outcome {
            SolverOutcome::Unsat => "unsat".to_string(),
            SolverOutcome::Sat => "sat".to_string(),
            SolverOutcome::Unknown => "unknown".to_string(),
            SolverOutcome::Error(e) => format!("(error {e})"),
        };
        let result = SolverResult { outcome, stdout, stderr: String::new() };
        let ctx = &*input.context_id;
        info!(
            %ctx,
            sent = %truncate(&input.formula, 160),
            outcome = ?result.outcome,
            recv = %truncate(&result.stdout, 120),
            "fuzz solver",
        );
        result
    }
}

// ---- Rust analyzer -------------------------------------------------------

pub struct FuzzRustAnalyzer;

impl Fuzz<RustAnalyzerRequest, RustAnalyzerResponse> for FuzzRustAnalyzer {
    fn fuzz(&self, input: &RustAnalyzerRequest, u: &mut Unstructured) -> RustAnalyzerResponse {
        let resp = match input {
            RustAnalyzerRequest::ListFunctions { files, .. } => {
                let file = files.first().cloned().unwrap_or_else(|| "src/lib.rs".into());
                let count = u.int_in_range(1..=3usize).unwrap_or(1);
                let fns = (0..count)
                    .map(|i| FunctionInfo {
                        id: FunctionId::new(file.clone(), &format!("f{i}"), None, 1),
                        body: String::new(),
                        start_line: 1,
                        end_line: 5,
                        docs: String::new(),
                        has_guarantees: true,
                    })
                    .collect();
                RustAnalyzerResponse::FunctionList(fns)
            }
            RustAnalyzerRequest::GetFunctionCode { function_id } => {
                let code = format!(
                    "fn {}() {{\n    let x = 1;\n    assert!(x > 0);\n}}\n",
                    function_id.name
                );
                RustAnalyzerResponse::FunctionCode(code)
            }
            RustAnalyzerRequest::GetCalledFunctions { .. } => {
                let n = u.int_in_range(0..=2usize).unwrap_or(0);
                let called = (0..n)
                    .map(|i| CalledFunction {
                        name: format!("helper{i}"),
                        file: None,
                        start_line: None,
                    })
                    .collect();
                RustAnalyzerResponse::CalledFunctionList(called)
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
        };
        info!(req = ?input, recv = ?resp, "fuzz rust-analyzer");
        resp
    }
}

// ---- Git -----------------------------------------------------------------

pub struct FuzzGit;

impl Fuzz<GitRequest, GitResponse> for FuzzGit {
    fn fuzz(&self, input: &GitRequest, _u: &mut Unstructured) -> GitResponse {
        let (success, output) = match input {
            GitRequest::WalkRustFiles { .. } => (true, "src/lib.rs\n".to_string()),
            GitRequest::CurrentCommitHash => {
                (true, "deadbeefcafebabe1234567890abcdef12345678".to_string())
            }
            GitRequest::FindChangedRustFiles { .. } => (true, String::new()),
            _ => (true, String::new()),
        };
        let resp = GitResponse { success, output };
        info!(req = ?input, success = resp.success, recv = %truncate(&resp.output, 120), "fuzz git");
        resp
    }
}

// ---- File system ---------------------------------------------------------

pub struct FuzzFileSystem;

impl Fuzz<FileSystemRequest, FileSystemResponse> for FuzzFileSystem {
    fn fuzz(&self, input: &FileSystemRequest, _u: &mut Unstructured) -> FileSystemResponse {
        let resp = FileSystemResponse {
            path: input.dir.join(&input.filename),
        };
        info!(filename = %input.filename, recv = %resp.path.display(), "fuzz filesystem");
        resp
    }
}

// ---- Python --------------------------------------------------------------

pub struct FuzzPython;

impl Fuzz<PythonRequest, PythonResponse> for FuzzPython {
    fn fuzz(&self, input: &PythonRequest, u: &mut Unstructured) -> PythonResponse {
        // ~10% of the time (non-exhausted only) return empty smtlib so
        // translate_formula bails and check_formula marks the formula Unknown.
        // Exhausted (0) -> normal echo, preserving clean termination.
        let empty = u.int_in_range(0u8..=9).unwrap_or(0) == 9;
        let smtlib = if empty {
            String::new()
        } else if input.script.trim().is_empty() {
            "(check-sat)".to_string()
        } else {
            input.script.clone()
        };
        let resp = PythonResponse { smtlib, explanation: String::new() };
        info!(
            sent_len = input.script.len(),
            recv = %truncate(&resp.smtlib, 160),
            "fuzz python",
        );
        resp
    }
}

// ---- Agent ---------------------------------------------------------------

pub struct FuzzAgent;

impl Fuzz<AgentRequest, AgentResponse> for FuzzAgent {
    fn fuzz(&self, _input: &AgentRequest, u: &mut Unstructured) -> AgentResponse {
        // Only the top of the range triggers RETRY; the range start (0) — the
        // value `int_in_range` returns on an exhausted stream — maps to a bug
        // report. So an exhausted stream never retries and ProblemAnalyzer
        // drains. Non-exhausted: ~10% RETRY keeps expected cycles geometric.
        let retry = u.int_in_range(0u8..=9).unwrap_or(0) == 9;
        // Per the ProblemAnalyzer contract, a non-RETRY response IS a bug
        // report. The mock must therefore emit a bug description (not a
        // non-committal "no bug" string, which the state machine would still
        // classify as a bug, producing misleading traces).
        let stdout = if retry {
            "RETRY".to_string()
        } else {
            "Bug: SAT counterexample indicates a possible precondition violation or overly strong assertion.".to_string()
        };
        let resp = AgentResponse { stdout, success: true };
        info!(retry, recv = %resp.stdout, "fuzz agent");
        resp
    }
}

// ---------------------------------------------------------------------------
// Provider bundle
// ---------------------------------------------------------------------------

/// Owns all seven fuzz providers, all sharing one byte stream, and lends out a
/// [`Providers`] borrowing them.
pub struct FuzzProviders {
    llm: ContextFuzzProvider<FuzzLlm>,
    solver: ContextFuzzProvider<FuzzSolver>,
    rust_analyzer: FuzzProvider<FuzzRustAnalyzer, RustAnalyzerRequest, RustAnalyzerResponse>,
    git: FuzzProvider<FuzzGit, GitRequest, GitResponse>,
    filesystem: FuzzProvider<FuzzFileSystem, FileSystemRequest, FileSystemResponse>,
    python: FuzzProvider<FuzzPython, PythonRequest, PythonResponse>,
    agent: FuzzProvider<FuzzAgent, AgentRequest, AgentResponse>,
}

impl FuzzProviders {
    pub fn new(data: Vec<u8>) -> Self {
        let stream = fuzz_stream_from_bytes(data);
        Self {
            llm: ContextFuzzProvider::new(FuzzLlm, Arc::clone(&stream)),
            solver: ContextFuzzProvider::new(FuzzSolver, Arc::clone(&stream)),
            rust_analyzer: FuzzProvider::with_stream(FuzzRustAnalyzer, Arc::clone(&stream)),
            git: FuzzProvider::with_stream(FuzzGit, Arc::clone(&stream)),
            filesystem: FuzzProvider::with_stream(FuzzFileSystem, Arc::clone(&stream)),
            python: FuzzProvider::with_stream(FuzzPython, Arc::clone(&stream)),
            agent: FuzzProvider::with_stream(FuzzAgent, stream),
        }
    }

    pub fn providers(&self) -> Providers<'_> {
        Providers {
            llm: &self.llm,
            solver: &self.solver,
            rust_analyzer: &self.rust_analyzer,
            git: &self.git,
            filesystem: &self.filesystem,
            python: &self.python,
            agent: &self.agent,
        }
    }
}

// ---------------------------------------------------------------------------
// State-machine driver
// ---------------------------------------------------------------------------

/// Create a temp project dir with a `.git` marker so the `Initializer` takes
/// its fast path (no real git operations; the mock git provider handles them).
pub fn setup_temp_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create temp project dir");
    std::fs::create_dir_all(dir.path().join(".git")).expect("failed to create .git marker");
    dir
}

/// Run the full deductive-check state machine against the fuzz providers.
/// Panics (phase-assertion failures, type-invariant violations, parser edge
/// cases) propagate so libFuzzer records them as crashes.
pub fn run_state_machine(data: &[u8]) {
    let project = setup_temp_project();
    let providers = FuzzProviders::new(data.to_vec());
    let pm = DefaultDeductivePieceManager::default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to build tokio runtime");
    let _ = rt.block_on(machine::run(
        project.path().to_str().expect("non-utf8 temp path"),
        &providers.providers(),
        &pm,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Runs `run_state_machine` on a worker thread with a hard deadline. A hang
    /// (e.g. an unterminated global cycle) surfaces as `Err("HANG")`.
    fn run_with_deadline(data: Vec<u8>, deadline_sec: u64) -> Result<(), &'static str> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_state_machine(&data);
            }))
            .is_ok();
            let _ = tx.send(ok);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(deadline_sec)) {
            Ok(true) => Ok(()),
            Ok(false) => Err("panicked"),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("HANG"),
            Err(_) => Err("worker died"),
        }
    }

    /// An exhausted stream must terminate: all defaults converge to
    /// judge=REASONABLE + solver=Unsat + agent=no-RETRY, so every piece closes.
    #[test]
    fn terminates_on_empty_input() {
        run_with_deadline(Vec::new(), 15).expect("empty input did not terminate");
    }

    #[test]
    fn terminates_on_small_inputs() {
        for data in [vec![0u8; 16], vec![0xffu8; 16], vec![42u8; 64], vec![128u8; 256]] {
            let len = data.len();
            run_with_deadline(data, 15).unwrap_or_else(|e| panic!("len {len}: {e}"));
        }
    }

    /// Pick the largest corpus input (richest execution) for a representative trace.
    fn pick_corpus_input() -> Option<Vec<u8>> {
        let dir = std::path::PathBuf::from("corpus/state_machine");
        let mut entries: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|e| Some((e.metadata().ok()?.len(), e.path())))
            .collect();
        entries.sort_by_key(|(len, _)| *len);
        entries.last().map(|(_, p)| std::fs::read(p).ok())?
    }

    /// Runs one state-machine execution with a tracing subscriber writing the
    /// full `deductive_check` event flow to a file. Run with:
    ///   cargo +nightly test trace_one_run -- --nocapture
    #[test]
    fn trace_one_run() {
        use tracing_subscriber::{fmt, prelude::*, EnvFilter};

        let trace_path = std::path::PathBuf::from(
            std::env::var("FUZZ_TRACE_PATH")
                .unwrap_or_else(|_| "/tmp/opencode/fuzz_trace.log".to_string()),
        );
        if let Some(parent) = trace_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file = std::fs::File::create(&trace_path).expect("create trace file");

        // Thread-local default subscriber; run_state_machine runs on this same
        // thread (current-thread runtime), so all events are captured.
        let _guard = tracing_subscriber::registry()
            .with(EnvFilter::new(
                "deductive_check=info,deductive_check_fuzz=info,refactor_check_core=warn,warn",
            ))
            .with(fmt::layer().with_writer(file).with_ansi(false).with_target(false))
            .set_default();

        let data = match std::env::var("FUZZ_INPUT_PATH") {
            Ok(p) => std::fs::read(&p)
                .unwrap_or_else(|e| panic!("FUZZ_INPUT_PATH read failed ({p}): {e}")),
            Err(_) => pick_corpus_input().unwrap_or_else(|| vec![123u8; 200]),
        };
        eprintln!("tracing input of {} bytes -> {}", data.len(), trace_path.display());
        run_state_machine(&data);
        eprintln!("trace written to {}", trace_path.display());
    }
}
