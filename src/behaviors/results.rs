use anyhow::Result;
use std::sync::Arc;
use futures::future;

use crate::llm;
use crate::provider::{LlmProvider, LlmRole, SolverProvider};
use crate::smt::extract_all_formulas;
use crate::states::*;
use crate::transitions;

use super::child_judge;

fn build_retry_messages(
    input_content: &str,
    formula_id: &str,
    feedback: &str,
    solver_stdout: &str,
    solver_stderr: &str,
) -> Vec<crate::llm::Message> {
    let prompt = format!(
        "Original refactoring:\n\n{}\n\n\
         The formula that failed:\n\n{}\n\n\
         Judge feedback: {}\n\n\
         Solver output: {}\n\n\
         Solver stderr: {}\n\n\
         Please provide ONE corrected SMT-LIB2 formula in a ```smt2 code block.",
        input_content, formula_id, feedback, solver_stdout, solver_stderr,
    );

    vec![
        llm::system_message(
            "You are an expert in formal verification. Fix the following SMT formula so \
             it correctly checks equivalence. Do NOT output any explanation. \
             Output ONLY the fixed formula in a single ```smt2 code block.",
        ),
        llm::user_message(&prompt),
    ]
}

fn build_retry_insist_messages(
    input_content: &str,
    formula_id: &str,
    feedback: &str,
    last_response: &str,
) -> Vec<crate::llm::Message> {
    vec![
        llm::system_message(
            "You MUST output exactly one SMT-LIB2 formula in a single ```smt2 code block. \
             Do NOT include any explanations.",
        ),
        llm::user_message(&format!(
            "Your previous response contained no valid SMT formula.\n\
             Here it was:\n\n{last_response}\n\n\
             Original refactoring:\n\n{input_content}\n\n\
             Formula to fix:\n\n{formula_id}\n\n\
             Feedback: {feedback}\n\n\
             Try again. ONE complete formula in a ```smt2 code block.",
        )),
    ]
}

pub async fn execute_all(
    state: &WaitForResults,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<Vec<ChildDone>> {
    let futures = state
        .branches
        .iter()
        .map(|branch| run_branch(branch, llm, solver));
    let results: Vec<Vec<ChildDone>> = future::try_join_all(futures).await?;
    Ok(results.into_iter().flatten().collect())
}

async fn run_branch(
    branch: &FormulaBranch,
    llm: &dyn LlmProvider,
    solver: &dyn SolverProvider,
) -> Result<Vec<ChildDone>> {
    let input = Arc::clone(&branch.input_content);
    let verified = branch.verified.clone();
    let formula_id = branch.formula_id.clone();
    let retry_count = branch.retry_count;
    let mut phase = branch.phase.clone();

    loop {
        match phase {
            BranchPhase::WaitForSolver { formula } => {
                let result = solver.run(&formula).await?;
                match transitions::transition_solver(formula, result) {
                    BranchFromSolver::Judge(f, r) => {
                        phase = BranchPhase::WaitForJudge {
                            formula: f,
                            solver_result: r,
                        };
                    }
                    BranchFromSolver::Error(f, r) => {
                        return Ok(vec![ChildDone::Open(OpenItem {
                            formula: f,
                            reason: format!("Solver error: {}", r.stdout),
                            solver_stdout: r.stdout,
                            solver_stderr: r.stderr,
                        })]);
                    }
                }
            }
            BranchPhase::WaitForJudge {
                formula,
                solver_result,
            } => {
                let verdict =
                    child_judge::execute(&input, &formula, &solver_result, llm).await?;
                match transitions::transition_judge(
                    formula,
                    solver_result,
                    verdict,
                    retry_count,
                ) {
                    BranchFromJudge::Verified(piece) => {
                        return Ok(vec![ChildDone::Verified(piece)]);
                    }
                    BranchFromJudge::Retry {
                        formula: _,
                        feedback,
                        solver_stdout,
                        solver_stderr,
                    } => {
                        phase = BranchPhase::NeedFormula {
                            feedback: Some(feedback),
                            solver_stdout,
                            solver_stderr,
                            insist_pending: false,
                            insist_attempt: 0,
                            last_response: None,
                        };
                    }
                    BranchFromJudge::Exhausted {
                        formula,
                        feedback,
                        solver_stdout,
                        solver_stderr,
                    } => {
                        return Ok(vec![ChildDone::Open(OpenItem {
                            formula,
                            reason: format!("Branch retry exhausted: {feedback}"),
                            solver_stdout,
                            solver_stderr,
                        })]);
                    }
                }
            }
            BranchPhase::NeedFormula {
                ref feedback,
                ref solver_stdout,
                ref solver_stderr,
                insist_pending,
                insist_attempt,
                last_response,
            } => {
                let fb = feedback.as_deref().unwrap_or("");
                let role = LlmRole::Fixer;

                let response = if insist_pending {
                    let prev = last_response.as_deref().unwrap_or("");
                    llm.chat(
                        role,
                        build_retry_insist_messages(&input, &formula_id, fb, prev),
                    )
                    .await?
                } else {
                    llm.chat(
                        role,
                        build_retry_messages(&input, &formula_id, fb, solver_stdout, solver_stderr),
                    )
                    .await?
                };

                let formulas = extract_all_formulas(&response);
                match transitions::transition_need_formula(formulas, insist_attempt) {
                    BranchFromNeedFormula::Proceed(formula) => {
                        phase = BranchPhase::WaitForSolver { formula };
                    }
                    BranchFromNeedFormula::Insist => {
                        phase = BranchPhase::NeedFormula {
                            feedback: Some(fb.to_string()),
                            solver_stdout: solver_stdout.clone(),
                            solver_stderr: solver_stderr.clone(),
                            insist_pending: true,
                            insist_attempt: insist_attempt + 1,
                            last_response: Some(response),
                        };
                    }
                    BranchFromNeedFormula::FanOut(formulas) => {
                        let sub = formulas.into_iter().map(|f| FormulaBranch {
                            input_content: Arc::clone(&input),
                            verified: verified.clone(),
                            formula_id: f.clone(),
                            phase: BranchPhase::WaitForSolver { formula: f },
                            retry_count,
                        });
                        let subs: Vec<FormulaBranch> = sub.collect();
                        let sub_results: Vec<Vec<ChildDone>> = future::try_join_all(
                            subs.iter().map(|b| run_branch(b, llm, solver)),
                        )
                        .await?;
                        let mut all = Vec::new();
                        for chunks in sub_results {
                            all.extend(chunks);
                        }
                        if all.is_empty() {
                            all.push(ChildDone::Open(OpenItem {
                                formula: formula_id,
                                reason: "All sub-branches exhausted".to_string(),
                                solver_stdout: solver_stdout.clone(),
                                solver_stderr: solver_stderr.clone(),
                            }));
                        }
                        return Ok(all);
                    }
                    BranchFromNeedFormula::Exhausted(reason) => {
                        return Ok(vec![ChildDone::Open(OpenItem {
                            formula: formula_id,
                            reason,
                            solver_stdout: solver_stdout.clone(),
                            solver_stderr: solver_stderr.clone(),
                        })]);
                    }
                }
            }
        }
    }
}