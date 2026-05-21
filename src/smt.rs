use anyhow::{Context, Result};
use async_trait::async_trait;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{debug, info, instrument, trace, warn};

use crate::provider::SolverProvider;

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
    let blocks = find_fenced_smt_blocks(response);
    if blocks.len() == 1 {
        blocks.into_iter().next().unwrap()
    } else if blocks.is_empty() {
        response.trim().to_string()
    } else {
        blocks.into_iter().next().unwrap()
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
        if bt_count >= 3 && !in_fence {
            in_fence = true;
            fence_backticks = bt_count;
            fence_buf.clear();
            continue;
        }
        if in_fence && bt_count >= 3 && bt_count >= fence_backticks {
            in_fence = false;
            let candidate = fence_buf.trim().to_string();
            if !candidate.is_empty() {
                blocks.push(candidate);
            }
            continue;
        }
        if in_fence {
            fence_buf.push_str(line);
            fence_buf.push('\n');
        }
    }

    blocks
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
        None | Some(_) => SolverOutcome::Unknown,
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
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

    let mut child = tokio::process::Command::new(solver_path)
        .args(solver_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn solver: {solver_path}"))?;

    {
        let mut stdin = child.stdin.take().context("Failed to open stdin")?;
        stdin.write_all(formula.as_bytes()).await
            .context("Failed to write formula to solver stdin")?;
    }

    let mut stdout_pipe = child.stdout.take().context("Failed to open stdout")?;
    let mut stderr_pipe = child.stderr.take().context("Failed to open stderr")?;

    let read_stdout = tokio::task::spawn(async move {
        let mut buf = Vec::new();
        stdout_pipe.read_to_end(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    });
    let read_stderr = tokio::task::spawn(async move {
        let mut buf = Vec::new();
        stderr_pipe.read_to_end(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    });

    let child = Arc::new(Mutex::new(child));
    let child_for_wait = child.clone();

let status = tokio::select! {
        status = async move { child_for_wait.lock().await.wait().await } => Some(status),
        () = tokio::time::sleep(timeout) => {
            warn!(elapsed_ms = start.elapsed().as_millis(), "solver timed out, killing process");
            {
                let mut guard = child.lock().await;
                let _ = guard.kill().await;
                let _ = guard.wait().await;
            }
            None
        }
    };

    let elapsed = start.elapsed();

    let Some(status) = status else {
        let _ = read_stdout.await;
        let _ = read_stderr.await;
        let timeout_msg = format!("solver timed out after {}s", timeout.as_secs());
        return Ok(SolverResult {
            outcome: SolverOutcome::Unknown,
            stdout: timeout_msg,
            stderr: String::new(),
        });
    };

    let status = status.context("Failed to wait for solver process")?;
    let stdout = read_stdout.await
        .context("stdout reader task panicked")?
        .context("Failed to read solver stdout")?;
    let stderr = read_stderr.await
        .context("stderr reader task panicked")?
        .context("Failed to read solver stderr")?;

    let stdout = bytes_to_string(stdout);
    let stderr = bytes_to_string(stderr);
    let outcome = parse_solver_outcome(status.code(), &stdout, &stderr);
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
    async fn run(&self, formula: &str, _piece: Option<&crate::states::CodePiece>) -> Result<SolverResult> {
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
