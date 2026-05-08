use std::fmt::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, instrument, warn};

use crate::llm::{LlmClient, LlmConfig, Message, system_message, user_message};
use crate::provider::{AgentResult, FormulaResult, LlmProvider, LlmRole, SolverProvider};
use crate::smt::{SolverOutcome, SolverResult, Z3Solver, extract_all_formulas};

pub use crate::smt::DEFAULT_SOLVER_TIMEOUT_SECS;

pub const MAX_ITERATIONS: usize = 30;
pub const MAX_INSIST_ATTEMPTS: usize = 50;
pub const MAX_JUDGE_ATTEMPTS: usize = 5;

pub const JUDGE_REASONABLE: &str = "REASONABLE";

pub enum JudgeVerdict {
    Reasonable,
    Retry(String), // explanation of what is wrong with the formula
}

pub struct AgentConfig {
    pub llm_config: LlmConfig,
    pub solver_path: String,
    pub solver_args: Vec<String>,
    pub solver_timeout_secs: u64,
}

struct VerifiedPiece {
    formula: String,
    outcome: SolverOutcome,
}

struct OpenItem {
    formula: String,
    reason: String,
    solver_stdout: String,
    solver_stderr: String,
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
    println!("=== Overall Equivalent ===\n{}\n", result.overall_equivalent);
    println!("=== Verified Formulas ===\n{}/{} reasonable (SAT: {}, UNSAT: {}, UNKNOWN: {})\n",
        result.formulas.len(),
        result.formulas.len() + result.open_count,
        result.reasonable_sat,
        result.reasonable_unsat,
        result.reasonable_unknown
    );
    println!("=== Open Pieces ===\n{}\n", result.open_count);
    for f in &result.formulas {
        println!("--- Formula ---\n{}\nOutcome: {:?}, Verdict: {}\n",
            f.formula, f.outcome, f.verdict);
        if let Some(ref explanation) = f.explanation {
            println!("--- Explanation ---\n{explanation}\n");
        }
    }
    Ok(())
}

#[instrument(skip_all, fields(input_bytes = input_content.len()))]
pub async fn run_with_providers(
    input_content: &str,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<AgentResult> {
    info!(bytes = input_content.len(), "input loaded");

    let mut verified: Vec<VerifiedPiece> = Vec::new();
    let mut open: Vec<OpenItem> = Vec::new();

    for iteration in 0..MAX_ITERATIONS {
        info!(iteration = iteration + 1, "starting iteration");

        // 1. Generate batch
        let messages = build_generation_messages(input_content, &verified, &open);
        let response = llm.chat(LlmRole::Primary, messages).await?;

        debug!(response, "formula batch received");

        let mut formulas = extract_all_formulas(&response);
        if formulas.is_empty() {
            warn!("no formulas found in response, insisting");
            let one = insist_on_at_least_one_formula(llm, input_content, &response).await?;
            formulas = vec![one];
        }

        info!(count = formulas.len(), "formula batch extracted");

        // 2. Run solvers in parallel
        let solver_futures = formulas.iter().map(|f| solver.run(f));
        let solver_results = futures::future::join_all(solver_futures).await;

        // 3. Partition into errors vs ok
        let mut to_judge: Vec<(String, SolverResult)> = Vec::new();
        for (formula, result) in formulas.iter().zip(solver_results) {
            let result = result?; // propagate spawn errors
            let is_error = matches!(result.outcome, SolverOutcome::Error(_));
            if is_error {
                warn!(formula = %formula, error = %result.stdout, "solver error");
                open.push(OpenItem {
                    formula: formula.clone(),
                    reason: format!("Solver error: {}", result.stdout),
                    solver_stdout: result.stdout,
                    solver_stderr: result.stderr,
                });
            } else {
                info!(
                    outcome = match &result.outcome {
                        SolverOutcome::Sat => "sat",
                        SolverOutcome::Unsat => "unsat",
                        SolverOutcome::Unknown => "unknown",
                        SolverOutcome::Error(_) => unreachable!(),
                    },
                    "solver outcome"
                );
                to_judge.push((formula.clone(), result));
            }
        }

        // 4. Judge all non-error formulas in parallel
        if !to_judge.is_empty() {
            let judge_futures = to_judge.iter().map(|(f, r)| judge_analysis(llm, input_content, f, r));
            let judge_results = futures::future::join_all(judge_futures).await;

            let mut freshly_verified = Vec::new();

            for ((formula, solver_result), verdict) in to_judge.iter().zip(judge_results) {
                let verdict = verdict?;
                match verdict {
                    JudgeVerdict::Reasonable => {
                        if matches!(solver_result.outcome, SolverOutcome::Sat) {
                            info!(formula = %formula, "NOT EQUIVALENT: found SAT in verified piece");
                        }
                        info!(outcome = ?solver_result.outcome, "piece verified (judge REASONABLE)");
                        freshly_verified.push(formula.clone());
                        verified.push(VerifiedPiece {
                            formula: formula.clone(),
                            outcome: solver_result.outcome.clone(),
                        });
                    }
                    JudgeVerdict::Retry(feedback) => {
                        warn!(formula = %formula, feedback, "judge RETRY, moving to open items");
                        open.push(OpenItem {
                            formula: formula.clone(),
                            reason: "Our verification engineer did not think the formula can correctly verify the equivalence. ".to_string() + &feedback,
                            solver_stdout: solver_result.stdout.clone(),
                            solver_stderr: solver_result.stderr.clone(),
                        });
                    }
                }
            }

            // Remove open items that were just successfully re-verified in this batch
            open.retain(|item| !freshly_verified.iter().any(|f| f == &item.formula));
        }

        // 5. Stopping condition
        if open.is_empty() {
            info!("all pieces verified, converged");
            let result = build_result(verified, 0);
            return Ok(explain_formulas(input_content, llm, result).await);
        }

        info!(open = open.len(), verified = verified.len(), "iteration complete, items remain open");
    }

    // Max iterations reached
    let result = build_result(verified, open.len());
    info!(
        open = result.open_count,
        sat = result.reasonable_sat,
        unsat = result.reasonable_unsat,
        unknown = result.reasonable_unknown,
        "max iterations reached, partial result"
    );
    Ok(explain_formulas(input_content, llm, result).await)
}

fn build_result(verified: Vec<VerifiedPiece>, open_count: usize) -> AgentResult {
    let reasonable_sat = verified.iter().filter(|v| matches!(v.outcome, SolverOutcome::Sat)).count();
    let reasonable_unsat = verified.iter().filter(|v| matches!(v.outcome, SolverOutcome::Unsat)).count();
    let reasonable_unknown = verified.iter().filter(|v| matches!(v.outcome, SolverOutcome::Unknown)).count();
    let overall_equivalent = open_count == 0 && reasonable_sat == 0 && reasonable_unknown == 0;

    AgentResult {
        formulas: verified.into_iter().map(|v| FormulaResult {
            formula: v.formula,
            outcome: v.outcome.clone(),
            verdict: JUDGE_REASONABLE.to_string(),
            explanation: None,
        }).collect(),
        overall_equivalent,
        open_count,
        reasonable_sat,
        reasonable_unsat,
        reasonable_unknown,
    }
}

#[instrument(skip_all)]
async fn explain_formulas(input_content: &str, llm: &dyn LlmProvider, mut result: AgentResult) -> AgentResult {
    let needs_explanation: Vec<usize> = result.formulas.iter().enumerate()
        .filter(|(_, f)| matches!(f.outcome, SolverOutcome::Sat | SolverOutcome::Unknown))
        .map(|(i, _)| i)
        .collect();

    if needs_explanation.is_empty() {
        return result;
    }

    info!(count = needs_explanation.len(), "generating bug explanations");

    let futures = needs_explanation.iter().map(|&i| {
        let formula = result.formulas[i].formula.clone();
        let outcome = result.formulas[i].outcome.clone();
        async move {
            let messages = build_explanation_messages(input_content, &formula, &outcome);
            let response = llm.chat(LlmRole::Primary, messages).await;
            (i, response)
        }
    });

    let explanations = futures::future::join_all(futures).await;

    for (i, response) in explanations {
        match response {
            Ok(text) => {
                info!("explanation received for formula {}", i);
                result.formulas[i].explanation = Some(text);
            }
            Err(e) => {
                warn!(error = %e, "failed to get explanation for formula {}", i);
                result.formulas[i].explanation = None;
            }
        }
    }

    result
}

fn build_explanation_messages(input_content: &str, formula: &str, outcome: &SolverOutcome) -> Vec<Message> {
    let system = if matches!(outcome, SolverOutcome::Sat) {
        "You are an expert in program analysis and formal verification. \
         The SMT solver found a counterexample (SAT) for an equivalence formula. \
         Explain the source of the bug: what behavioral difference between \
         the 'before' and 'after' code does the counterexample represent?"
    } else {
        "You are an expert in program analysis and formal verification. \
         The SMT solver could not decide (UNKNOWN) for an equivalence formula. \
         Evaluate whether there is likely a real bug, or whether the formula \
         is simply too complex for the solver. Highlight that the solver's result is INCONCLUSIVE."
    };

    let prompt = format!(
        "Original refactoring:\n\n{}\n\n\
         SMT formula:\n\n{}\n\n\
         Solver outcome: {}\n\n\
         Please provide your analysis.",
        input_content,
        formula,
        match outcome {
            SolverOutcome::Sat => "SAT (counterexample found)",
            SolverOutcome::Unknown => "UNKNOWN (solver could not decide)",
            _ => unreachable!(),
        }
    );

    vec![system_message(system), user_message(&prompt)]
}

fn build_generation_messages(
    input_content: &str,
    verified: &[VerifiedPiece],
    open: &[OpenItem],
) -> Vec<Message> {
    let mut messages = Vec::new();

    messages.push(system_message(
        "You are an expert in formal verification. Check whether the 'before' and 'after' code are equivalent. \
         You may generate ONE OR MORE complete SMT-LIB2 formulas, each checking equivalence of a specific \
         sub-piece (e.g., a loop body, a helper function, a conditional branch). You can also generate one \
         formula for the entire code. \
         \n\nRules:\n\
         - Output one or more complete standalone SMT-LIB2 formulas.\n\
         - Each formula must be complete (include set-logic, declarations, assertions, check-sat).\n\
         - Use (check-sat) and optionally (get-model).\n\
         - Put each formula in a separate ```smt2 code block.\n\
         - If the functions are equivalent, each formula should be unsatisfiable.\n\
         - If any formula is satisfiable, the overall refactoring is NOT equivalent."
    ));

    let mut content = format!("Here is the refactoring description:\n\n{input_content}\n\n");

    if !verified.is_empty() {
        content.push_str("Pieces that have already been verified — you do NOT need to recheck these:\n\n");
        for (i, piece) in verified.iter().enumerate() {
            let _ = write!(content, "Piece {} ({}):\n{piece}\n",
                i + 1,
                match &piece.outcome {
                    SolverOutcome::Sat => "SAT — NOT EQUIVALENT",
                    SolverOutcome::Unsat => "UNSAT — equivalent",
                    SolverOutcome::Unknown => "UNKNOWN — inconclusive",
                    SolverOutcome::Error(_) => unreachable!(),
                },
                piece = piece.formula
            );
        }
        content.push('\n');
    }

    if !open.is_empty() {
        content.push_str("Pieces that still need work:\n\n");
        for (i, item) in open.iter().enumerate() {
            let _ = write!(
                content,
                "Open piece {}:\nFormula:\n{}\nIssue: {}\n",
                i + 1,
                item.formula,
                item.reason
            );
            if !item.solver_stdout.is_empty() {
                let _ = write!(content, "Solver output:\n{}\n", item.solver_stdout);
            }
            if !item.solver_stderr.is_empty() {
                let _ = write!(content, "Standard error:\n{}\n", item.solver_stderr);
            }
            content.push('\n');
        }
    }

    if verified.is_empty() && open.is_empty() {
        content.push_str("Generate SMT-LIB2 formula(s) checking equivalence. Start with one formula covering the whole refactoring. If the solver times out, split the code into smaller analogous pieces (e.g., loop bodies, helper functions, conditionals) and generate one formula per piece.\n");
    } else {
        content.push_str("Please generate new or improved formulas for the unverified pieces above. You can also add new pieces if you think some behavior has not been checked yet.\n");
    }

    messages.push(user_message(&content));
    messages
}

#[instrument(skip_all)]
async fn insist_on_at_least_one_formula(
    llm: &dyn LlmProvider,
    input_content: &str,
    previous_response: &str,
) -> Result<String> {
    let mut attempt = 0;
    let mut last_response = previous_response.to_string();

    loop {
        attempt += 1;
        if attempt > MAX_INSIST_ATTEMPTS {
            anyhow::bail!("Failed to extract any SMT formula after {MAX_INSIST_ATTEMPTS} attempts");
        }

        debug!(attempt, "insisting on at least one formula");

        let messages = vec![
            system_message(
                "You MUST output at least ONE SMT-LIB2 formula. \
                 Put each formula in a separate ```smt2 code block. \
                 Each formula must be complete with set-logic, declarations, assertions, and check-sat."
            ),
            user_message(&format!(
                "Your previous response did not contain any valid SMT formula. \
                 Here was your previous response:\n\n{last_response}\n\n\
                 Here is the input file:\n\n{input_content}\n\n\
                 Please try again. Output at least ONE valid SMT-LIB2 formula in a ```smt2 code block."
            )),
        ];

        let response = llm.chat(LlmRole::Primary, messages).await?;
        let formulas = extract_all_formulas(&response);
        if formulas.is_empty() {
            last_response = response;
            continue;
        }
        // If multiple, just take the first one
        if formulas.len() > 1 {
            info!(attempt, count = formulas.len(), "multiple formulas found after insistence, using first");
        } else {
            info!(attempt, "formula extracted after insistence");
        }
        return Ok(formulas.into_iter().next().unwrap());
    }
}

#[instrument(skip_all)]
async fn judge_analysis(
    llm: &dyn LlmProvider,
    input_content: &str,
    formula: &str,
    solver_result: &SolverResult,
) -> Result<JudgeVerdict> {
    let mut attempts = 0;
    let mut last_response = String::new();

    loop {
        attempts += 1;
        if attempts > MAX_JUDGE_ATTEMPTS {
            anyhow::bail!("Judge failed to give a clear verdict after {MAX_JUDGE_ATTEMPTS} attempts");
        }

        debug!(attempt = attempts, "asking judge for verdict");

        let prompt = if attempts == 1 {
            format!(
                "Original refactoring:\n\n{input_content}\n\n\
                 Formula checking one piece:\n\n{formula}\n\n\
                 Solver result:\n{}\n\n\
                 Is this formula correctly checking equivalence? \
                 Answer ONLY with the single word REASONABLE if yes. \
                 Otherwise explain what is wrong with the formula.",
                solver_result.stdout
            )
        } else {
            format!(
                "Original refactoring:\n\n{input_content}\n\n\
                 Formula checking one piece:\n\n{formula}\n\n\
                 Solver result:\n{}\n\n\
                 Your previous answer was: '{last_response}'. \
                 You MUST answer ONLY with REASONABLE or explain what is wrong.",
                solver_result.stdout
            )
        };

        let messages = vec![
            system_message(
                "You are a judge evaluating an SMT-based equivalence check. \
                 If the formula correctly and completely checks equivalence, answer ONLY with the single word REASONABLE. \
                 If the formula does NOT correctly check equivalence, explain what is wrong: \
                 Does it fail to represent some behavior? Is the abstraction too coarse or too fine? \
                 Are assertions missing important cases? Provide your explanation concisely."
            ),
            user_message(&prompt),
        ];

        let response = llm.chat(LlmRole::Judge, messages).await?;
        let trimmed = response.trim();
        let upper = trimmed.to_uppercase();

        if upper.starts_with(JUDGE_REASONABLE) {
            return Ok(JudgeVerdict::Reasonable);
        }

        // Extract explanation: strip leading "RETRY" if present, otherwise use full text
        let explanation = if upper.len() > 4 && upper[..5] == *"RETRY" {
            trimmed[5..].trim().to_string()
        } else {
            trimmed.to_string()
        };

        if !explanation.is_empty() {
            return Ok(JudgeVerdict::Retry(explanation));
        }

        warn!(response = %response, "judge gave unclear answer, insisting");
        last_response = response;
    }
}
