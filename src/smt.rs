use anyhow::{Context, Result};
use std::process::Output;

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
    let formulas = collect_smt_formulas(response);
    if formulas.len() == 1 {
        Some(formulas.into_iter().next().unwrap())
    } else {
        None
    }
}

fn collect_smt_formulas(text: &str) -> Vec<String> {
    let mut formulas = Vec::new();

    // First try to extract from fenced code blocks
    let mut in_fence = false;
    let mut fence_len = 0;
    let mut fence_buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") && !in_fence {
            in_fence = true;
            fence_len = trimmed.len();
            fence_buf.clear();
            continue;
        }
        if in_fence && trimmed.starts_with("```") && trimmed.len() == fence_len {
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

    // If we found formulas in fences, return those
    if !formulas.is_empty() {
        return formulas;
    }

    // Otherwise, try to find bare SMT content
    // Collect contiguous blocks of SMT-like lines
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
        || line.starts_with("(declare-fun")
        || line.starts_with("(declare-const")
        || line.starts_with("(assert")
        || line.starts_with("(check-sat")
        || line.starts_with("(get-model)")
        || line.starts_with("(get-value")
        || line.starts_with("(define-fun")
        || line.starts_with("(define-const")
        || line.starts_with("(push)")
        || line.starts_with("(pop)")
        || line.starts_with("(exit)")
        || line.starts_with(";")
        || line.is_empty()
        || line.starts_with("(set-option")
        || line.starts_with("(set-info")
}

fn looks_like_smt(text: &str) -> bool {
    let text = text.trim();
    text.contains("(set-logic")
        || (text.contains("(declare-") && text.contains("(assert"))
}

pub async fn run_solver(solver_path: &str, formula: &str) -> Result<SolverResult> {
    let output = tokio::task::spawn_blocking({
        let solver_path = solver_path.to_string();
        let formula = formula.to_string();
        move || -> Result<Output> {
            use std::process::{Command, Stdio};
            let mut child = Command::new(&solver_path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("Failed to spawn solver: {solver_path}"))?;
            {
                let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
                use std::io::Write;
                stdin.write_all(formula.as_bytes())?;
            }
            let output = child.wait_with_output()?;
            Ok(output)
        }
    })
    .await
    .context("Solver task panicked")??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let outcome = if !stderr.trim().is_empty()
        && (stderr.contains("error") || stderr.contains("Error") || stderr.contains("ERROR"))
    {
        SolverOutcome::Error(stderr.trim().to_string())
    } else {
        match stdout.trim() {
            s if s.starts_with("sat") => SolverOutcome::Sat,
            s if s.starts_with("unsat") => SolverOutcome::Unsat,
            s if s.starts_with("unknown") => SolverOutcome::Unknown,
            s => {
                // Check if there are error-like messages in stdout too
                if s.contains("error") || s.contains("Error") || s.contains("ERROR") {
                    SolverOutcome::Error(s.to_string())
                } else {
                    SolverOutcome::Unknown
                }
            }
        }
    };

    Ok(SolverResult {
        outcome,
        stdout,
        stderr,
    })
}