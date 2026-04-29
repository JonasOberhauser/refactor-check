use anyhow::{Context, Result};

use crate::llm::{LlmClient, LlmConfig, system_message, user_message};
use crate::smt::{SolverResult, SolverOutcome, extract_smt_formula, run_solver};

const MAX_ITERATIONS: u32 = 10;

pub struct AgentConfig {
    pub llm_config: LlmConfig,
    pub solver_path: String,
}

pub struct HistoryEntry {
    pub formula: String,
    pub solver_stdout: String,
    pub solver_stderr: String,
    pub had_error: bool,
}

pub async fn run(input_file: &str, config: AgentConfig) -> Result<()> {
    let input_content = std::fs::read_to_string(input_file)
        .with_context(|| format!("Failed to read input file: {input_file}"))?;

    let client = LlmClient::new(config.llm_config);
    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut current_formula: Option<String> = None;

    for iteration in 0..MAX_ITERATIONS {
        eprintln!("--- Iteration {} ---", iteration + 1);

        let messages = build_generation_messages(&input_content, &history, current_formula.is_none());
        let response = client.chat_primary(messages).await?;
        eprintln!("LLM response received ({} bytes)", response.len());

        let formula = match extract_smt_formula(&response) {
            Some(f) => f,
            None => {
                eprintln!("No single SMT formula found, insisting...");
                current_formula = Some(insist_on_formula(&client, &input_content, &response).await?);
                continue;
            }
        };

        eprintln!("Formula extracted ({} bytes)", formula.len());

        let solver_result = run_solver(&config.solver_path, &formula).await?;
        let had_error = matches!(solver_result.outcome, SolverOutcome::Error(_));

        eprintln!(
            "Solver outcome: {}",
            match &solver_result.outcome {
                SolverOutcome::Sat => "sat",
                SolverOutcome::Unsat => "unsat",
                SolverOutcome::Unknown => "unknown",
                SolverOutcome::Error(e) => e,
            }
        );

        if had_error {
            eprintln!("Solver error detected, skipping judge and looping back...");
            let analysis_messages = build_error_analysis_messages(
                &input_content, &history, &formula, &solver_result,
            );
            let analysis = client.chat_primary(analysis_messages).await?;
            eprintln!("Error analysis received ({} bytes)", analysis.len());

            history.push(HistoryEntry {
                formula: formula.clone(),
                solver_stdout: solver_result.stdout.clone(),
                solver_stderr: solver_result.stderr.clone(),
                had_error,
            });
            current_formula = Some(formula);
            continue;
        }

        let analysis_messages = build_success_analysis_messages(&formula, &solver_result);
        let analysis = client.chat_primary(analysis_messages).await?;
        eprintln!("Analysis received ({} bytes)", analysis.len());

        let verdict = judge_analysis(&client, &analysis).await?;
        eprintln!("Judge verdict: {}", verdict);

        if verdict == "YES" {
            println!("=== LLM Analysis ===\n{analysis}\n");
            println!("=== SMT Formula ===\n{formula}");
            return Ok(());
        }

        history.push(HistoryEntry {
            formula: formula.clone(),
            solver_stdout: solver_result.stdout.clone(),
            solver_stderr: solver_result.stderr.clone(),
            had_error,
        });
        current_formula = Some(formula);
    }

    anyhow::bail!("Exceeded maximum iterations ({MAX_ITERATIONS}) without convergence");
}

fn build_generation_messages(
    input_content: &str,
    history: &[HistoryEntry],
    is_first: bool,
) -> Vec<(String, String)> {
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
        for (i, entry) in history.iter().enumerate() {
            content.push_str(&format!("--- Attempt {} ---\n", i + 1));
            content.push_str(&format!("Formula:\n{}\n", entry.formula));
            if entry.had_error {
                content.push_str(&format!("Solver ERROR:\n{}\n{}\n", entry.solver_stdout, entry.solver_stderr));
            } else {
                content.push_str(&format!("Solver output:\n{}\n", entry.solver_stdout));
            }
            content.push('\n');
        }
        content.push_str("Generate a new single complete SMT-LIB2 formula that fixes the issues above.");
        messages.push(user_message(&content));
    }

    messages
}

async fn insist_on_formula(
    client: &LlmClient,
    input_content: &str,
    previous_response: &str,
) -> Result<String> {
    let mut attempt = 0;
    let max_insist = 5;
    let mut last_response = previous_response.to_string();

    loop {
        attempt += 1;
        if attempt > max_insist {
            anyhow::bail!("Failed to extract a single SMT formula after {max_insist} attempts");
        }

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

        let response = client.chat_primary(messages).await?;
        if let Some(formula) = extract_smt_formula(&response) {
            return Ok(formula);
        }
        last_response = response;
    }
}

fn build_error_analysis_messages(
    input_content: &str,
    history: &[HistoryEntry],
    current_formula: &str,
    solver_result: &SolverResult,
) -> Vec<(String, String)> {
    let mut content = format!("Here is the input file:\n\n{input_content}\n\n");
    content.push_str("Full history of prior attempts:\n\n");
    for (i, entry) in history.iter().enumerate() {
        content.push_str(&format!("--- Attempt {} ---\n", i + 1));
        content.push_str(&format!("Formula:\n{}\n", entry.formula));
        content.push_str(&format!("Solver output:\n{}\n{}\n\n", entry.solver_stdout, entry.solver_stderr));
    }
    content.push_str(&format!("Current formula:\n{current_formula}\n\n"));
    content.push_str(&format!(
        "Current solver response:\nstdout:\n{}\nstderr:\n{}\n\n\
         The solver returned an error. Analyze what went wrong and whether a new formula should be generated. \
         Explain your reasoning.",
        solver_result.stdout, solver_result.stderr
    ));

    vec![
        system_message(
            "You are an expert in formal verification and SMT-LIB2. \
             Analyze the solver error and determine what went wrong with the SMT formula. \
             Should a new formula be generated, or is the solver response reasonable despite the error?"
        ),
        user_message(&content),
    ]
}

fn build_success_analysis_messages(
    current_formula: &str,
    solver_result: &SolverResult,
) -> Vec<(String, String)> {
    vec![
        system_message(
            "You are an expert in formal verification and SMT-LIB2. \
             Analyze the solver output for the given SMT formula and determine if the result is reasonable. \
             A reasonable result means the formula correctly encodes the equivalence check \
             and the solver's answer (sat/unsat/unknown) makes sense. \
             If the result is NOT reasonable (e.g., the formula is wrong, or the solver result \
             doesn't actually prove equivalence), say a new formula should be generated."
        ),
        user_message(&format!(
            "Current formula:\n{current_formula}\n\n\
             Solver output:\n{}\n\n\
             Is this solver response reasonable, or should a new formula be generated? Explain your reasoning.",
            solver_result.stdout
        )),
    ]
}

async fn judge_analysis(client: &LlmClient, analysis: &str) -> Result<String> {
    let mut attempts = 0;
    let max_attempts = 5;
    let mut last_analysis = analysis.to_string();

    loop {
        attempts += 1;
        if attempts > max_attempts {
            anyhow::bail!("Judge failed to give a clear YES/NO after {max_attempts} attempts");
        }

        let messages = vec![
            system_message(
                "You are a judge evaluating an SMT-based equivalence check. \
                 The SMT solver ran SUCCESSFULLY (no errors). \
                 Read the analysis below and determine: \
                 does the analysis confirm that the solver result is CORRECT and REASONABLE, \
                 meaning the formula properly encodes the equivalence check \
                 and the solver's answer (sat/unsat/unknown) is a valid conclusion? \
                 \n\nAnswer ONLY with YES or NO. Nothing else."
            ),
            user_message(&last_analysis),
        ];

        let response = client.chat_judge(messages).await?;
        let upper = response.trim().to_uppercase();
        let trimmed = upper.trim_start_matches(|c: char| !c.is_alphabetic());

        if trimmed.starts_with("YES") {
            return Ok("YES".to_string());
        } else if trimmed.starts_with("NO") {
            return Ok("NO".to_string());
        } else {
            eprintln!("Judge gave unclear answer: {}, insisting...", response);
            last_analysis = format!(
                "{analysis}\n\n\
                 Your previous answer was: '{response}'. \
                 You MUST answer ONLY with YES or NO. Does the analysis confirm the solver result is correct and reasonable?"
            );
        }
    }
}