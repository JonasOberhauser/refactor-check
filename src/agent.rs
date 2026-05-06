use std::fmt::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, instrument, warn};

use crate::llm::{LlmClient, LlmConfig, Message, system_message, user_message};
use crate::provider::{AgentResult, LlmProvider, LlmRole, SolverProvider};
use crate::smt::{SolverOutcome, SolverResult, Z3Solver, extract_smt_formula};

pub use crate::smt::DEFAULT_SOLVER_TIMEOUT_SECS;

pub const MAX_ITERATIONS: usize = 30;
pub const MAX_INSIST_ATTEMPTS: usize = 50;
pub const MAX_JUDGE_ATTEMPTS: usize = 5;

pub const JUDGE_REASONABLE: &str = "REASONABLE";
pub const JUDGE_RETRY: &str = "RETRY";

pub struct AgentConfig {
    pub llm_config: LlmConfig,
    pub solver_path: String,
    pub solver_args: Vec<String>,
    pub solver_timeout_secs: u64,
}

pub struct HistoryEntry {
    pub formula: String,
    pub solver_stdout: String,
    pub solver_stderr: String,
    pub had_error: bool,
}

fn format_history(history: &[HistoryEntry]) -> String {
    let mut content = String::new();
    for (i, entry) in history.iter().enumerate() {
        let _ = writeln!(content, "--- Attempt {} ---", i + 1);
        let _ = write!(content, "Formula:\n{}\n", entry.formula);
        if entry.had_error {
            let _ = write!(content, "Solver ERROR:\n{}\n{}\n", entry.solver_stdout, entry.solver_stderr);
        } else {
            let _ = write!(content, "Solver output:\n{}\n", entry.solver_stdout);
        }
        content.push('\n');
    }
    content
}

pub async fn run(input_file: &str, config: AgentConfig) -> Result<()> {
    let input_content = std::fs::read_to_string(input_file)
        .with_context(|| format!("Failed to read input file: {input_file}"))?;

    let llm = LlmClient::new(config.llm_config);
    let solver = Z3Solver::with_config(
        config.solver_path,
        config.solver_args,
        Duration::from_secs(config.solver_timeout_secs),
    );

    let result = run_with_providers(&input_content, &llm, &solver).await?;
    println!("=== Solver Outcome ===\n{:?}\n", result.solver_outcome);
    println!("=== Solver Output ===\n{}\n", result.solver_stdout);
    println!("=== SMT Formula ===\n{}", result.formula);
    Ok(())
}

#[instrument(skip_all, fields(input_bytes = input_content.len()))]
pub async fn run_with_providers(
    input_content: &str,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<AgentResult> {
    info!(bytes = input_content.len(), "input loaded");

    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut current_formula: Option<String> = None;

    for iteration in 0..MAX_ITERATIONS {
        info!(iteration = iteration + 1, "starting iteration");

        let messages = build_generation_messages(input_content, &history, current_formula.is_none());
        let response = llm.chat(LlmRole::Primary, messages).await?;

        debug!(response, "formula received");

        let formula = if let Some(f) = extract_smt_formula(&response) {
            f
        } else {
            warn!("no single SMT formula found, insisting");
            insist_on_formula(llm, input_content, &response).await?
        };

        debug!(formula_bytes = formula.len(), "formula extracted");
        debug!(%formula, "extracted formula content");

        let solver_result = solver.run(&formula).await?;
        let had_error = matches!(solver_result.outcome, SolverOutcome::Error(_));

        info!(
            outcome = match &solver_result.outcome {
                SolverOutcome::Sat => "sat",
                SolverOutcome::Unsat => "unsat",
                SolverOutcome::Unknown => "unknown",
                SolverOutcome::Error(e) => e,
            },
            "solver outcome"
        );

        if had_error {
            warn!("solver error, looping back to generate new formula");

            history.push(HistoryEntry {
                formula: formula.clone(),
                solver_stdout: solver_result.stdout.clone(),
                solver_stderr: solver_result.stderr.clone(),
                had_error,
            });
            current_formula = Some(formula);
            continue;
        }

        let verdict = judge_analysis(llm, &formula, &solver_result).await?;
        info!(verdict = %verdict, "judge verdict");

        if verdict == JUDGE_REASONABLE {
            return Ok(AgentResult {
                formula,
                solver_outcome: solver_result.outcome,
                solver_stdout: solver_result.stdout,
            });
        }

        history.push(HistoryEntry {
            formula: formula.clone(),
            solver_stdout: solver_result.stdout.clone(),
            solver_stderr: solver_result.stderr.clone(),
            had_error,
        });
        current_formula = Some(formula);
    }

    let summary = summarize_failure(llm, input_content, &history).await?;
    anyhow::bail!("Exceeded maximum iterations ({MAX_ITERATIONS}) without convergence.\n\n{summary}");
}

fn build_generation_messages(
    input_content: &str,
    history: &[HistoryEntry],
    is_first: bool,
) -> Vec<Message> {
    let mut messages = Vec::new();

    messages.push(system_message(
        "You are an expert in formal verification and SMT-LIB2. \
         Your task is to generate a SINGLE, COMPLETE, STANDALONE SMT-LIB2 formula \
         that checks whether the 'before' and 'after' functions described in the input \
         are semantically equivalent. \
         \n\nRules:\n\
         - Output exactly ONE SMT-LIB2 formula.\n\
         - The formula must be complete and standalone (include set-logic, declarations, assertions, check-sat).\n\
         - Use (check-sat) and optionally (get-model).\n\
         - Do NOT output multiple formulas.\n\
         - Put the formula in a ```smt2 code block for easy extraction.\n\
         - If the functions are equivalent, the formula should be unsatisfiable (the negation of equivalence is unsat)."
    ));

    if is_first {
        messages.push(user_message(&format!(
            "Here is the refactoring description:\n\n{input_content}\n\n\
             Generate a single complete SMT-LIB2 formula checking equivalence of the before and after functions."
        )));
    } else {
        let mut content = format!("Here is the refactoring description:\n\n{input_content}\n\n");
        content.push_str("Previous attempts failed. Here is the full history:\n\n");
        content.push_str(&format_history(history));
        content.push_str("Generate a new single complete SMT-LIB2 formula that fixes the issues above.");
        messages.push(user_message(&content));
    }

    messages
}

#[instrument(skip_all)]
async fn insist_on_formula(
    llm: &dyn LlmProvider,
    input_content: &str,
    previous_response: &str,
) -> Result<String> {
    let mut attempt = 0;
    let mut last_response = previous_response.to_string();

    loop {
        attempt += 1;
        if attempt > MAX_INSIST_ATTEMPTS {
            anyhow::bail!("Failed to extract a single SMT formula after {MAX_INSIST_ATTEMPTS} attempts");
        }

        debug!(attempt, "insisting on single formula");

        let messages = vec![
            system_message(
                "You MUST output exactly ONE SMT-LIB2 formula. Not zero, not multiple. \
                 Put it in a single ```smt2 code block. \
                 The formula must be complete with set-logic, declarations, assertions, and check-sat."
            ),
            user_message(&format!(
                "Your previous response did not contain exactly one SMT formula. \
                 Here was your previous response:\n\n{last_response}\n\n\
                 Here is the input file:\n\n{input_content}\n\n\
                 Please try again. Output exactly ONE SMT-LIB2 formula in a ```smt2 code block."
            )),
        ];

        let response = llm.chat(LlmRole::Primary, messages).await?;
        if let Some(formula) = extract_smt_formula(&response) {
            info!(attempt, "formula extracted after insistence");
            return Ok(formula);
        }
        last_response = response;
    }
}

#[instrument(skip_all)]
async fn summarize_failure(
    llm: &dyn LlmProvider,
    input_content: &str,
    history: &[HistoryEntry],
) -> Result<String> {
    let mut content = format!("Here is the input file:\n\n{input_content}\n\n");
    content.push_str("After multiple attempts, the agent failed to converge. Here is the full history:\n\n");
    content.push_str(&format_history(history));
    content.push_str("Please summarize why the agent failed to converge and what could be done differently.");

    let messages = vec![
        system_message(
            "You are an expert in formal verification and SMT-LIB2. \
             Summarize concisely why the equivalence check failed to converge \
             and suggest what could be improved."
        ),
        user_message(&content),
    ];

    llm.chat(LlmRole::Primary, messages).await
}

#[instrument(skip_all)]
async fn judge_analysis(llm: &dyn LlmProvider, formula: &str, solver_result: &SolverResult) -> Result<String> {
    let mut attempts = 0;
    let mut last_response = String::new();

    loop {
        attempts += 1;
        if attempts > MAX_JUDGE_ATTEMPTS {
            anyhow::bail!("Judge failed to give a clear {JUDGE_REASONABLE}/{JUDGE_RETRY} after {MAX_JUDGE_ATTEMPTS} attempts");
        }

        debug!(attempt = attempts, "asking judge for verdict");

        let prompt = if attempts == 1 {
            format!(
                "Formula:\n{formula}\n\n\
                 Solver output:\n{}\n\n\
                 Is this solver response reasonable, or should a new formula be generated?",
                solver_result.stdout
            )
        } else {
            format!(
                "Formula:\n{formula}\n\n\
                 Solver output:\n{}\n\n\
                 Your previous answer was: '{last_response}'. \
                 You MUST answer ONLY with {JUDGE_REASONABLE} or {JUDGE_RETRY}. \
                 Is the solver response reasonable, or should a new formula be generated?",
                solver_result.stdout
            )
        };

        let messages = vec![
            system_message(
                "You are a judge evaluating an SMT-based equivalence check. \
                 The SMT solver ran SUCCESSFULLY (no errors). \
                 You are given the SMT formula and the solver's output. \
                 Determine: does the solver result correctly and reasonably \
                 prove or disprove the equivalence of the 'before' and 'after' functions? \
                 Consider whether the formula properly encodes the equivalence check \
                 and whether the solver's answer (sat/unsat/unknown) is a valid conclusion. \
                 \n\nAnswer ONLY with REASONABLE or RETRY. Nothing else."
            ),
            user_message(&prompt),
        ];

        let response = llm.chat(LlmRole::Judge, messages).await?;
        let upper = response.trim().to_uppercase();
        let trimmed = upper.trim_start_matches(|c: char| !c.is_alphabetic());

        if trimmed.starts_with(JUDGE_REASONABLE) {
            return Ok(JUDGE_REASONABLE.to_string());
        }
        if trimmed.starts_with(JUDGE_RETRY) {
            return Ok(JUDGE_RETRY.to_string());
        }
        warn!(response = %response, "judge gave unclear answer, insisting");
        last_response = response;
    }
}