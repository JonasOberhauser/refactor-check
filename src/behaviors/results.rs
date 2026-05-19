use anyhow::Result;
use std::sync::Arc;
use futures::future;

use crate::consts::{MAX_BRANCH_RETRIES, MAX_INSIST_ATTEMPTS};
use crate::provider::{LlmProvider, LlmRole, SolverProvider};
use crate::smt::{SolverOutcome, extract_all_formulas};
use crate::states::*;

use super::child_judge;
use super::child_solver;

fn build_branch_retry_messages(
    input_content: &str,
    formula: &str,
    feedback: &str,
    solver_stdout: &str,
    solver_stderr: &str,
) -> Vec<crate::llm::Message> {
    use crate::llm;

    let system = "You are an expert in formal verification. Fix the following SMT formula so it correctly checks equivalence. \
                  Do NOT output any explanation. Output ONLY the fixed formula in a single ```smt2 code block.";

    let prompt = format!(
        "Original refactoring:\n\n{}\n\n\
         The formula that failed:\n\n{}\n\n\
         Judge feedback: {}\n\n\
         Solver output: {}\n\n\
         Solver stderr: {}\n\n\
         Please provide ONE corrected SMT-LIB2 formula in a ```smt2 code block.",
        input_content, formula, feedback, solver_stdout, solver_stderr,
    );

    vec![llm::system_message(system), llm::user_message(&prompt)]
}

pub async fn execute_all(
    state: &WaitForResults,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<Vec<ChildDone>> {
    let futures = state.branches.iter().map(|branch| run_branch(branch, llm, solver));
    let results: Vec<Vec<ChildDone>> = future::try_join_all(futures).await?;
    Ok(results.into_iter().flatten().collect())
}

async fn run_branch(
    branch: &FormulaBranch,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<Vec<ChildDone>> {
    let mut current = FormulaBranch {
        input_content: Arc::clone(&branch.input_content),
        verified: branch.verified.clone(),
        region: branch.region.clone(),
        state: branch.state.clone(),
        retry_count: branch.retry_count,
        config: branch.config.clone(),
    };

    loop {
        match &current.state {
            BranchState::WaitForSolver { formula } => {
                let result = child_solver::execute(formula, solver).await?;
                if matches!(result.outcome, SolverOutcome::Error(_)) {
                    return Ok(vec![ChildDone::Open(OpenItem {
                        formula: formula.clone(),
                        reason: format!("Solver error: {}", result.stdout),
                        solver_stdout: result.stdout,
                        solver_stderr: result.stderr,
                    })]);
                }
                current = FormulaBranch {
                    state: BranchState::WaitForJudge {
                        formula: formula.clone(),
                        solver_result: result,
                    },
                    ..current
                };
            }
            BranchState::WaitForJudge {
                formula,
                solver_result,
            } => {
                let verdict = child_judge::execute(
                    &current.config.llm_config,
                    &current.input_content,
                    formula,
                    solver_result,
                    llm,
                )
                .await?;
                match verdict {
                    JudgeVerdict::Reasonable => {
                        return Ok(vec![ChildDone::Verified(VerifiedPiece {
                            formula: formula.clone(),
                            outcome: solver_result.outcome.clone(),
                        })]);
                    }
                    JudgeVerdict::Retry(feedback) => {
                        let new_retry = current.retry_count + 1;
                        if new_retry >= MAX_BRANCH_RETRIES {
                            return Ok(vec![ChildDone::Open(OpenItem {
                                formula: formula.clone(),
                                reason: format!(
                                    "Branch retry exhausted ({}): {}",
                                    MAX_BRANCH_RETRIES, feedback
                                ),
                                solver_stdout: solver_result.stdout.clone(),
                                solver_stderr: solver_result.stderr.clone(),
                            })]);
                        }
                        current = FormulaBranch {
                            state: BranchState::NeedFormula {
                                feedback: Some(feedback),
                                solver_stdout: solver_result.stdout.clone(),
                                solver_stderr: solver_result.stderr.clone(),
                            },
                            retry_count: new_retry,
                            ..current
                        };
                    }
                }
            }
            BranchState::NeedFormula {
                feedback,
                solver_stdout,
                solver_stderr,
            } => {
                let formula_name = match &current.region {
                    Region::Formula(f) => f.as_str(),
                    Region::Global => "",
                };
                let response = generate_retry_formula(
                    &current.input_content,
                    formula_name,
                    feedback.as_deref().unwrap_or(""),
                    solver_stdout,
                    solver_stderr,
                    llm,
                )
                .await?;
                let formulas = extract_all_formulas(&response);
                if formulas.is_empty() {
                    return Ok(vec![ChildDone::Open(OpenItem {
                        formula: formula_name.to_string(),
                        reason: "Branch generation produced no formula".to_string(),
                        solver_stdout: solver_stdout.clone(),
                        solver_stderr: solver_stderr.clone(),
                    })]);
                }
                if formulas.len() == 1 {
                    current = FormulaBranch {
                        state: BranchState::WaitForSolver {
                            formula: formulas.into_iter().next().unwrap(),
                        },
                        ..current
                    };
                } else {
                    let sub_branches: Vec<FormulaBranch> = formulas
                        .into_iter()
                        .map(|f| FormulaBranch {
                            input_content: Arc::clone(&current.input_content),
                            verified: current.verified.clone(),
                            region: Region::Formula(f.clone()),
                            state: BranchState::WaitForSolver { formula: f },
                            retry_count: current.retry_count,
                            config: current.config.clone(),
                        })
                        .collect();

                    let sub_futures = sub_branches
                        .iter()
                        .map(|b| run_branch(b, llm, solver));
                    let sub_results: Vec<Vec<ChildDone>> =
                        future::try_join_all(sub_futures).await?;

                    let mut all_done = Vec::new();
                    for chunks in sub_results {
                        all_done.extend(chunks);
                    }
                    if all_done.is_empty() {
                        all_done.push(ChildDone::Open(OpenItem {
                            formula: formula_name.to_string(),
                            reason: "All sub-branches exhausted".to_string(),
                            solver_stdout: solver_stdout.clone(),
                            solver_stderr: solver_stderr.clone(),
                        }));
                    }
                    return Ok(all_done);
                }
            }
        }
    }
}

async fn generate_retry_formula(
    input_content: &str,
    formula: &str,
    feedback: &str,
    solver_stdout: &str,
    solver_stderr: &str,
    llm: &dyn LlmProvider,
) -> Result<String> {
    let mut response = llm
        .chat(
            LlmRole::Fixer,
            build_branch_retry_messages(
                input_content,
                formula,
                feedback,
                solver_stdout,
                solver_stderr,
            ),
        )
        .await?;

    let mut insist_attempts = 0;
    while extract_all_formulas(&response).is_empty() {
        insist_attempts += 1;
        if insist_attempts > MAX_INSIST_ATTEMPTS {
            anyhow::bail!("Branch failed to produce valid formula after {MAX_INSIST_ATTEMPTS} insist attempts");
        }
        response = llm
            .chat(
                LlmRole::Fixer,
                vec![
                    crate::llm::system_message(
                        "You MUST output exactly one SMT-LIB2 formula in a single ```smt2 code block. Do NOT include any explanations.",
                    ),
                    crate::llm::user_message(&format!(
                        "Your previous response contained no valid SMT formula. \
                         Here it was:\n\n{response}\n\n\
                         Original formula to fix:\n{}\n\n\
                         Feedback: {}\n\n\
                         Try again. ONE complete formula in a ```smt2 code block.",
                        formula, feedback,
                    )),
                ],
            )
            .await?;
    }

    Ok(response)
}
