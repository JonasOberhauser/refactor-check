use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, instrument, trace, warn};

use crate::config_update::AppConfig;
use crate::context_id::ContextId;
use crate::error_gate::ErrorGate;
use crate::live_config::LiveConfig;
use crate::provider::{IOProvider, SolverRequest, WithContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolverOutcome {
    Sat,
    Unsat,
    Unknown,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct SolverResult {
    pub outcome: SolverOutcome,
    pub stdout: String,
    pub stderr: String,
}

pub fn extract_smt_formula(response: &str) -> Option<String> {
    trace!("extracting SMT formula from LLM response");
    let formula = extract_single_formula(response);
    debug!(bytes = formula.len(), "SMT formula extracted");
    if formula.is_empty() {
        None
    } else {
        Some(formula)
    }
}

pub fn extract_all_formulas(response: &str) -> Vec<String> {
    trace!("extracting all SMT formulas from LLM response");
    let formulas = find_fenced_smt_blocks(response);
    debug!(count = formulas.len(), "SMT formula candidates found");
    if formulas.is_empty() {
        let trimmed = response.trim().to_string();
        if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed]
        }
    } else {
        formulas
    }
}

pub fn extract_single_formula(response: &str) -> String {
    match find_fenced_smt_blocks(response).as_slice() {
        [single] => single.clone(),
        [] => response.trim().to_string(),
        many => {
            warn!(count = many.len(), "LLM returned multiple SMT formulas where one was expected");
            String::new()
        }
    }
}

fn find_fenced_smt_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_fence = false;
    let mut fence_backticks: usize = 0;
    let mut fence_buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        let bt_count = backtick_count(trimmed);
        if bt_count >= FENCE_BACKTICKS && !in_fence {
            in_fence = true;
            fence_backticks = bt_count;
            fence_buf.clear();
            continue;
        }
        if in_fence && bt_count >= FENCE_BACKTICKS && bt_count >= fence_backticks {
            in_fence = false;
            let candidate = fence_buf.trim().to_string();
            if !candidate.is_empty() {
                blocks.push(candidate);
            }
            continue;
        }
        if in_fence {
            fence_buf.push_str(line);
            fence_buf.push(FENCE_NEWLINE);
        }
    }

    blocks
}

fn backtick_count(line: &str) -> usize {
    let trimmed = line.trim();
    let mut count = 0;
    for ch in trimmed.chars() {
        if ch == FENCE_BACKTICK_CHAR {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn parse_solver_outcome(exit_code: Option<i32>, stdout: &str, stderr: &str) -> SolverOutcome {
    if let Some(code) = exit_code {
        if code != 0 {
            return SolverOutcome::Error(
                if stderr.trim().is_empty() {
                    format!("solver exited with code {code}\n{}", stdout.trim())
                } else {
                    format!("solver exited with code {code}\n{}", stderr.trim())
                },
            );
        }
    }

    if stdout.lines().any(|l| l.trim().starts_with(SIG_ERROR_LINE)) {
        return SolverOutcome::Error(stdout.trim().to_string());
    }

    let first_meaningful = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty());

    match first_meaningful {
        Some(line) if line.starts_with(PREFIX_UNSAT) => SolverOutcome::Unsat,
        Some(line) if line.starts_with(PREFIX_SAT) => SolverOutcome::Sat,
        Some(line) if line.starts_with(PREFIX_UNKNOWN) => SolverOutcome::Unknown,
        None | Some(_) => SolverOutcome::Unknown,
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

pub const DEFAULT_SOLVER_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolverConfig {
    pub solver_path: String,
    pub solver_args: Vec<String>,
    pub timeout_secs: u64,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            solver_path: DEFAULT_SOLVER_PATH.to_string(),
            solver_args: vec![SOLVER_STDIN_ARG.to_string()],
            timeout_secs: DEFAULT_SOLVER_TIMEOUT_SECS,
        }
    }
}

/// # Panics
///
/// Panics if a pipe-reader thread panicked (structural: the pipe is owned
/// exclusively by that thread, so a panic there means the runtime is broken).
/// Backticks that open/close a fenced block.
const FENCE_BACKTICKS: usize = 3;
/// Characters of the fence grammar.
const FENCE_BACKTICK_CHAR: char = '`';
const FENCE_NEWLINE: char = '\n';
/// Solver answers, recognized at line starts.
const SIG_ERROR_LINE: &str = "(error";
const PREFIX_UNSAT: &str = "unsat";
const PREFIX_SAT: &str = "sat";
const PREFIX_UNKNOWN: &str = "unknown";
/// Default solver binary and its stdin mode argument.
const DEFAULT_SOLVER_PATH: &str = "z3";
const SOLVER_STDIN_ARG: &str = "-in";
/// Poll interval while waiting for the solver to exit.
const SOLVER_POLL_MS: u64 = 50;
/// Pipe-failure contexts.
const CTX_STDIN: &str = "Failed to open stdin";
const CTX_WRITE_FORMULA: &str = "Failed to write formula to solver stdin";
const CTX_STDOUT: &str = "Failed to open stdout";
const CTX_STDERR: &str = "Failed to open stderr";
const CTX_WAIT: &str = "Failed to wait for solver process";

/// Runs the solver on `formula`, capturing both pipes.
///
/// # Panics
///
/// Panics if a pipe-reader thread panicked while draining solver output
/// (the join on the reader threads expects a result).
#[instrument(skip_all, fields(solver_path, %context_id))]
pub fn run_solver(
    solver_path: &str,
    solver_args: &[String],
    timeout: Duration,
    formula: &str,
    context_id: &ContextId,
) -> Result<SolverResult> {
    info!(formula_bytes = formula.len(), ?timeout, %context_id, "running SMT solver");
    let start = std::time::Instant::now();

    let mut child = std::process::Command::new(solver_path)
        .args(solver_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn solver: {solver_path}"))?;

    {
        let mut stdin = child.stdin.take().context(CTX_STDIN)?;
        stdin
            .write_all(formula.as_bytes())
            .context(CTX_WRITE_FORMULA)?;
        // stdin dropped here: closes the pipe, signaling EOF to the solver.
    }

    let mut stdout_pipe = child.stdout.take().context(CTX_STDOUT)?;
    let mut stderr_pipe = child.stderr.take().context(CTX_STDERR)?;

    // Reader threads own the pipes for the whole child lifetime — a pipe
    // buffer filling up can never deadlock the solver.
    let read_stdout = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _n = std::io::Read::read_to_end(&mut stdout_pipe, &mut buf);
        buf
    });
    let read_stderr = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _n = std::io::Read::read_to_end(&mut stderr_pipe, &mut buf);
        buf
    });

    // Deadline watchdog: try_wait poll, kill on expiry.
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context(CTX_WAIT)? {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            warn!(elapsed_ms = start.elapsed().as_millis(), %context_id, "solver timed out, killing process");
            let _killed = child.kill();
            let _reaped = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(SOLVER_POLL_MS));
    };

    let elapsed = start.elapsed();

    let Some(status) = status else {
        let _stdout = read_stdout.join();
        let _stderr = read_stderr.join();
        let timeout_msg = format!("solver timed out after {}s", timeout.as_secs());
        return Ok(SolverResult {
            outcome: SolverOutcome::Unknown,
            stdout: timeout_msg,
            stderr: String::new(),
        });
    };

    let stdout = read_stdout.join().expect("stdout reader thread panicked");
    let stderr = read_stderr.join().expect("stderr reader thread panicked");

    let stdout = bytes_to_string(stdout);
    let stderr = bytes_to_string(stderr);
    let outcome = parse_solver_outcome(status.code(), &stdout, &stderr);
    debug!(%stdout, %stderr, %context_id, "solver raw output");
    info!(
        ?outcome,
        elapsed_ms = elapsed.as_millis(),
        stdout_bytes = stdout.len(),
        stderr_bytes = stderr.len(),
        %context_id,
        "solver finished"
    );

    Ok(SolverResult {
        outcome,
        stdout,
        stderr,
    })
}

pub struct Z3Solver {
    config: Arc<LiveConfig<AppConfig>>,
    error_gate: Option<Arc<ErrorGate>>,
}

impl Z3Solver {
    #[must_use]
    pub fn with_live_config(config: Arc<LiveConfig<AppConfig>>) -> Self {
        Self { config, error_gate: None }
    }

    #[must_use]
    pub fn with_error_gate(mut self, gate: Arc<ErrorGate>) -> Self {
        self.error_gate = Some(gate);
        self
    }
}

impl IOProvider<SolverRequest, WithContext<SolverResult>> for Z3Solver {
    fn invoke(&self, input: SolverRequest) -> Result<WithContext<SolverResult>> {
        let SolverRequest { formula, context_id } = input;
        loop {
            let cfg = self.config.snapshot().1.solver;
            let result = run_solver(
                &cfg.solver_path,
                &cfg.solver_args,
                Duration::from_secs(cfg.timeout_secs),
                &formula,
                &context_id,
            )
;

            match result {
                Ok(result) => return Ok(WithContext { value: result, context_id }),
                Err(e) => {
                    if let Some(gate) = &self.error_gate {
                        gate.report_and_wait(&format!("{e:#}"))?;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture formulas for the extraction tests.
    const FIXTURE_FORMULA_ASSERT_X: &str = "(set-logic QF_LIA)\n(declare-fun x () Int)\n(assert (= x 0))\n(check-sat)";
    const FIXTURE_FORMULA_ASSERT_Y: &str = "(set-logic QF_LIA)\n(declare-fun y () Int)\n(assert (> y 5))\n(check-sat)";
    const FIXTURE_FORMULA_BARE: &str = "(set-logic QF_LIA)\n(check-sat)";
    const ASSERT_X_LINE: &str = "(assert (= x 0))";
    const ASSERT_Y_LINE: &str = "(assert (> y 5))";
    const WHITESPACE_ONLY: &str = "   ";
    const TWO_FORMULAS: usize = 2;
    const FIRST_FORMULA: usize = 0;
    const SECOND_FORMULA: usize = 1;
    const ONE_FORMULA: usize = 1;
    const FIXTURE_FORMULA_JEMALLOC_STYLE: &str = "\
(set-logic UF)

(declare-sort Emitter 0)

(declare-fun f_version (Emitter) Emitter)
(declare-fun f_config  (Emitter) Emitter)
(declare-fun f_system  (Emitter) Emitter)
(declare-fun f_opt     (Emitter) Emitter)
(declare-fun f_prof    (Emitter) Emitter)
(declare-fun f_arenas  (Emitter) Emitter)

(declare-fun h_config  (Emitter) Emitter)
(declare-fun h_system  (Emitter) Emitter)

(declare-fun emit (Emitter Int) Emitter)

(define-fun op_cfg_dict_begin () Int 100)
(define-fun op_cfg_dict_end   () Int 114)

(define-fun f_config_detail ((e Emitter)) Emitter
  (let ((e1 (emit e op_cfg_dict_begin)))
  (let ((e2 (emit e1 op_cfg_dict_end)))
    e2))))

(define-fun h_config_detail ((e Emitter)) Emitter
  (let ((e1 (emit e op_cfg_dict_begin)))
  (let ((e2 (emit e1 op_cfg_dict_end)))
    e2))))

(assert (forall ((e Emitter)) (= (h_config_detail e) (f_config_detail e))))
(assert (forall ((e Emitter)) (= (h_config e) (f_config_detail e))))
(assert (forall ((e Emitter)) (= (f_config e) (f_config_detail e))))
(assert (forall ((e Emitter)) (= (h_system e) (f_system e))))
(assert (forall ((e Emitter)) (= (h_opt e) (f_opt e))))
(assert (forall ((e Emitter)) (= (h_prof e) (f_prof e))))
(assert (forall ((e Emitter)) (= (h_arenas e) (f_arenas e))))

(define-fun before ((e Emitter)) Emitter
  (f_arenas (f_prof (f_opt (f_system (f_config (f_version e)))))))

(define-fun after ((e Emitter)) Emitter
  (h_arenas (h_prof (h_opt (h_system (h_config (f_version e)))))))

(assert (exists ((e Emitter)) (not (= (before e) (after e)))))

(check-sat)
(get-model)";
    const FIXTURE_FORMULA_EQUATIONS: &str = "\n(set-logic UF)\n\n(declare-sort Emitter 0)\n\n(declare-fun f_version (Emitter) Emitter)\n(declare-fun f_config  (Emitter) Emitter)\n\n(assert (forall ((e Emitter)) (= (h_config e) (f_config e))))\n\n(check-sat)\n(get-model)";

    #[test]
    fn test_extract_formula_with_prose_and_backticks() {
        let formula = FIXTURE_FORMULA_EQUATIONS;

        let response = format!(
            "Looking at this refactoring, the \"before\" version is a monolithic \
             `stats_general_print` function.\n\n\
             ```smt2\n{}\n```\n\n\
             **Expected result: UNSAT**",
            formula
        );

        let result = extract_smt_formula(&response);
        assert!(result.is_some(), "should find exactly one formula");
        let extracted = result.unwrap();
        assert!(extracted.contains("(set-logic UF)"));
        assert!(extracted.contains("(check-sat)"));
    }

    #[test]
    fn test_extract_jemalloc_style_response() {
        let formula = FIXTURE_FORMULA_JEMALLOC_STYLE;

        let response = format!(
            "Looking at this refactoring, the \"before\" version is a monolithic \
             `stats_general_print` function that performs all operations inline, while the \
             \"after\" version decomposes it into helper functions called in the same order.\n\n\
             ```smt2\n{}\n```\n\n\
             **Expected result: UNSAT** — The formula is unsatisfiable because:\n\n\
             1. **Same operation sequence**: Both versions call `f_version` first.\n\
             2. **Helper functions match inline sections**: Each extracted helper function \
             performs exactly the same emitter API calls as its corresponding inline code.\n\
             3. **No cross-section variable dependencies**: All local variables are written \
             before being read within each section.\n\
             4. **Same conditional behavior**: All conditions are evaluated the same way in \
             both versions.",
            formula
        );

        let result = extract_smt_formula(&response);
        assert!(result.is_some(), "should find exactly one formula, but found 0 or >1");
        let extracted = result.unwrap();
        assert!(extracted.contains("(set-logic UF)"));
        assert!(extracted.contains("(declare-sort Emitter 0)"));
        assert!(extracted.contains("(check-sat)"));
    }

    #[test]
    fn test_extract_single_formula_rejects_multiple_blocks() {
        let formula_a = FIXTURE_FORMULA_ASSERT_X;
        let formula_b = FIXTURE_FORMULA_ASSERT_Y;
        let response = format!(
            "Here are two formulas:\n\n```smt2\n{a}\n```\n\nAnd the second:\n\n```smt2\n{b}\n```",
            a = formula_a,
            b = formula_b,
        );
        let result = extract_single_formula(&response);
        assert!(result.is_empty(), "should return empty string when multiple blocks found, got: {result}");
    }

    #[test]
    fn test_extract_all_formulas_multiple_blocks() {
        let formula_a = FIXTURE_FORMULA_ASSERT_X;
        let formula_b = FIXTURE_FORMULA_ASSERT_Y;
        let response = format!(
            "```smt2\n{a}\n```\nSome text\n\n```smt2\n{b}\n```",
            a = formula_a,
            b = formula_b,
        );
        let formulas = extract_all_formulas(&response);
        assert_eq!(formulas.len(), TWO_FORMULAS);
        assert!(formulas[FIRST_FORMULA].contains(ASSERT_X_LINE));
        assert!(formulas[SECOND_FORMULA].contains(ASSERT_Y_LINE));
    }

    #[test]
    fn test_extract_all_formulas_no_blocks_returns_bare_text() {
        let bare = FIXTURE_FORMULA_BARE;
        let formulas = extract_all_formulas(bare);
        assert_eq!(formulas.len(), ONE_FORMULA);
        assert_eq!(formulas[FIRST_FORMULA], bare.trim());
    }

    #[test]
    fn test_extract_all_formulas_empty_response() {
        let formulas = extract_all_formulas(WHITESPACE_ONLY);
        assert!(formulas.is_empty());
    }
}

