use anyhow::{Context, Result};
use async_trait::async_trait;
use std::io::Write;
use std::process::Output;
use std::time::Duration;
use tracing::{debug, info, instrument, trace, warn};

use crate::provider::SolverProvider;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolverOutcome {
    Sat,
    Unsat,
    Unknown,
    Error(String),
}

pub struct SolverResult {
    pub outcome: SolverOutcome,
    pub stdout: String,
    pub stderr: String,
}

pub fn extract_smt_formula(response: &str) -> Option<String> {
    trace!("extracting SMT formula from LLM response");
    let formulas = collect_smt_formulas(response);
    debug!(count = formulas.len(), "SMT formula candidates found");
    if formulas.len() == 1 {
        Some(formulas.into_iter().next().unwrap())
    } else {
        None
    }
}

fn backtick_count(line: &str) -> usize {
    let trimmed = line.trim();
    let mut count = 0;
    for ch in trimmed.chars() {
        if ch == '`' {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn collect_smt_formulas(text: &str) -> Vec<String> {
    let mut formulas = Vec::new();

    let mut in_fence = false;
    let mut fence_backticks: usize = 0;
    let mut fence_buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        let bt_count = backtick_count(trimmed);
        if bt_count >= 3 && !in_fence {
            in_fence = true;
            fence_backticks = bt_count;
            fence_buf.clear();
            continue;
        }
        if in_fence && bt_count >= 3 && bt_count >= fence_backticks {
            in_fence = false;
            let candidate = fence_buf.trim().to_string();
            if looks_like_smt(&candidate) {
                formulas.push(candidate);
            }
            continue;
        }
        if in_fence {
            fence_buf.push_str(line);
            fence_buf.push('\n');
        }
    }

    if !formulas.is_empty() {
        return formulas;
    }

    let mut current_block = String::new();
    let mut in_block = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if is_smt_line(trimmed) {
            if !in_block {
                in_block = true;
            }
            current_block.push_str(line);
            current_block.push('\n');
        } else if in_block {
            in_block = false;
            let candidate = current_block.trim().to_string();
            if looks_like_smt(&candidate) {
                formulas.push(candidate);
            }
            current_block.clear();
        }
    }

    if in_block {
        let candidate = current_block.trim().to_string();
        if looks_like_smt(&candidate) {
            formulas.push(candidate);
        }
    }

    formulas
}

fn is_smt_line(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("(set-logic")
        || line.starts_with("(declare-")
        || line.starts_with("(assert")
        || line.starts_with("(check-sat")
        || line.starts_with("(get-model)")
        || line.starts_with("(get-value")
        || line.starts_with("(define-fun")
        || line.starts_with("(push)")
        || line.starts_with("(pop)")
        || line.starts_with("(exit)")
        || line.starts_with(';')
        || line.is_empty()
        || line.starts_with("(set-option")
        || line.starts_with("(set-info")
}

fn looks_like_smt(text: &str) -> bool {
    let text = text.trim();
    text.contains("(set-logic")
        && text.contains("(check-sat")
        && (text.contains("(declare-") || text.contains("(define-fun"))
        && text.contains("(assert")
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

    if stdout.lines().any(|l| l.trim().starts_with("(error")) {
        return SolverOutcome::Error(stdout.trim().to_string());
    }

    let first_meaningful = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty());

    match first_meaningful {
        Some(line) if line.starts_with("unsat") => SolverOutcome::Unsat,
        Some(line) if line.starts_with("sat") => SolverOutcome::Sat,
        Some(line) if line.starts_with("unknown") => SolverOutcome::Unknown,
        None => SolverOutcome::Unknown,
        Some(_) => SolverOutcome::Unknown,
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

pub const DEFAULT_SOLVER_TIMEOUT_SECS: u64 = 60;

#[instrument(skip_all, fields(solver_path))]
pub async fn run_solver(
    solver_path: &str,
    solver_args: &[String],
    timeout: Duration,
    formula: &str,
) -> Result<SolverResult> {
    info!(formula_bytes = formula.len(), ?timeout, "running SMT solver");
    let start = std::time::Instant::now();

    let output = tokio::task::spawn_blocking({
        let solver_path = solver_path.to_string();
        let solver_args = solver_args.to_vec();
        let formula = formula.to_string();
        let timeout = timeout;
        move || -> Result<Option<Output>> {
            use std::process::{Command, Stdio};
            let mut child = Command::new(&solver_path)
                .args(&solver_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("Failed to spawn solver: {solver_path}"))?;
            {
                let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
                stdin.write_all(formula.as_bytes())?;
            }
            let timed_out = match child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => {
                    std::thread::sleep(timeout);
                    match child.try_wait() {
                        Ok(Some(_)) => false,
                        Ok(None) => {
                            let _ = child.kill();
                            true
                        }
                        Err(_) => false,
                    }
                }
                Err(_) => false,
            };
            if timed_out {
                return Ok(None);
            }
            let output = child.wait_with_output()?;
            Ok(Some(output))
        }
    })
    .await
    .context("Solver task panicked")??;

    let elapsed = start.elapsed();

    let Some(output) = output else {
        warn!(elapsed_ms = elapsed.as_millis(), "solver timed out");
        let timeout_msg = format!("solver timed out after {}s", timeout.as_secs());
        return Ok(SolverResult {
            outcome: SolverOutcome::Error(timeout_msg.clone()),
            stdout: timeout_msg,
            stderr: String::new(),
        });
    };

    let stdout = bytes_to_string(output.stdout);
    let stderr = bytes_to_string(output.stderr);
    let outcome = parse_solver_outcome(output.status.code(), &stdout, &stderr);
    debug!(%stdout, %stderr, "solver raw output");
    info!(
        ?outcome,
        elapsed_ms = elapsed.as_millis(),
        stdout_bytes = stdout.len(),
        stderr_bytes = stderr.len(),
        "solver finished"
    );

    Ok(SolverResult {
        outcome,
        stdout,
        stderr,
    })
}

pub struct Z3Solver {
    solver_path: String,
    solver_args: Vec<String>,
    timeout: Duration,
}

impl Z3Solver {
    #[must_use]
    pub fn new(solver_path: String) -> Self {
        Self::with_config(solver_path, vec!["-in".to_string()], Duration::from_secs(DEFAULT_SOLVER_TIMEOUT_SECS))
    }

    #[must_use]
    pub fn with_config(solver_path: String, solver_args: Vec<String>, timeout: Duration) -> Self {
        Self { solver_path, solver_args, timeout }
    }
}

#[async_trait]
impl SolverProvider for Z3Solver {
    async fn run(&self, formula: &str) -> Result<SolverResult> {
        run_solver(&self.solver_path, &self.solver_args, self.timeout, formula).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_formula_with_prose_and_backticks() {
        let formula = "\
(set-logic UF)

(declare-sort Emitter 0)

(declare-fun f_version (Emitter) Emitter)
(declare-fun f_config  (Emitter) Emitter)

(assert (forall ((e Emitter)) (= (h_config e) (f_config e))))

(check-sat)
(get-model)";

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
        let formula = "\
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
}
