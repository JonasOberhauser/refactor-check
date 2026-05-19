use std::fmt::Write;

use crate::llm::{Message, system_message, user_message};
use crate::smt::SolverOutcome;
use crate::states::{OpenItem, VerifiedPiece};

pub use crate::consts::*;
pub use crate::smt::DEFAULT_SOLVER_TIMEOUT_SECS;
pub use crate::states::AgentConfig;

pub async fn run_with_providers(
    input_content: &str,
    llm: &dyn crate::provider::LlmProvider,
    solver: &dyn crate::provider::SolverProvider,
) -> anyhow::Result<crate::provider::AgentResult> {
    let config = AgentConfig {
        llm_config: Default::default(),
        solver_path: "z3".to_string(),
        solver_args: vec!["-in".to_string()],
        solver_timeout_secs: DEFAULT_SOLVER_TIMEOUT_SECS,
    };
    crate::machine::run(input_content, config, llm, solver).await
}

pub async fn run(input_file: &str, config: AgentConfig) -> anyhow::Result<()> {
    let input_content = std::fs::read_to_string(input_file)
        .with_context(|| format!("Failed to read input file: {input_file}"))?;

    let llm = crate::llm::LlmClient::new(config.llm_config.clone());
    let solver = crate::smt::Z3Solver::with_config(
        config.solver_path.clone(),
        config.solver_args.clone(),
        std::time::Duration::from_secs(config.solver_timeout_secs),
    );

    let result = crate::machine::run(&input_content, config.clone(), &llm, &solver).await?;
    println!("=== Overall Equivalent ===\n{}\n", result.overall_equivalent);
    println!(
        "=== Verified Formulas ===\n{}/{} reasonable (SAT: {}, UNSAT: {}, UNKNOWN: {})\n",
        result.formulas.len(),
        result.formulas.len() + result.open_count,
        result.reasonable_sat,
        result.reasonable_unsat,
        result.reasonable_unknown,
    );
    println!("=== Open Pieces ===\n{}\n", result.open_count);
    for f in &result.formulas {
        println!(
            "--- Formula ---\n{}\nOutcome: {:?}, Verdict: {}\n",
            f.formula, f.outcome, f.verdict
        );
        if let Some(ref explanation) = f.explanation {
            println!("--- Explanation ---\n{explanation}\n");
        }
    }
    Ok(())
}

use anyhow::Context;

pub enum GenerationContext<'a> {
    Global {
        verified: &'a [VerifiedPiece],
        open: &'a [OpenItem],
    },
    LocalRetry {
        verified: &'a [VerifiedPiece],
        target: &'a OpenItem,
    },
}

pub fn build_generation_messages(
    input_content: &str,
    ctx: &GenerationContext,
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
         - If any formula is satisfiable, the overall refactoring is NOT equivalent.",
    ));

    let mut content = format!("Here is the refactoring description:\n\n{input_content}\n\n");

    match ctx {
        GenerationContext::Global { verified, open } => {
            if !verified.is_empty() {
                content
                    .push_str("Pieces that have already been verified — you do NOT need to recheck these:\n\n");
                for (i, piece) in verified.iter().enumerate() {
                    let _ = write!(
                        content,
                        "Piece {} ({}):\n{piece}\n",
                        i + 1,
                        match &piece.outcome {
                            SolverOutcome::Sat => "SAT — NOT EQUIVALENT",
                            SolverOutcome::Unsat => "UNSAT — equivalent",
                            SolverOutcome::Unknown => "UNKNOWN — inconclusive",
                            SolverOutcome::Error(_) => unreachable!(),
                        },
                        piece = piece.formula,
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
                        item.reason,
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
                content.push_str(
                    "Generate SMT-LIB2 formula(s) checking equivalence. Start with one formula covering the whole refactoring. If the solver times out, split the code into smaller analogous pieces (e.g., loop bodies, helper functions, conditionals) and generate one formula per piece.\n",
                );
            } else {
                content.push_str(
                    "Please generate new or improved formulas for the unverified pieces above. You can also add new pieces if you think some behavior has not been checked yet.\n",
                );
            }
        }
        GenerationContext::LocalRetry { verified, target } => {
            if !verified.is_empty() {
                content
                    .push_str("Pieces that have already been verified (for context only):\n\n");
                for (i, piece) in verified.iter().enumerate() {
                    let _ = write!(
                        content,
                        "Piece {} ({}):\n{piece}\n",
                        i + 1,
                        match &piece.outcome {
                            SolverOutcome::Sat => "SAT — NOT EQUIVALENT",
                            SolverOutcome::Unsat => "UNSAT — equivalent",
                            SolverOutcome::Unknown => "UNKNOWN — inconclusive",
                            SolverOutcome::Error(_) => unreachable!(),
                        },
                        piece = piece.formula,
                    );
                }
                content.push('\n');
            }

            let _ = write!(
                content,
                "Piece that needs improvement:\nFormula:\n{}\nIssue: {}\n",
                target.formula,
                target.reason,
            );
            if !target.solver_stdout.is_empty() {
                let _ = write!(content, "Solver output:\n{}\n", target.solver_stdout);
            }
            if !target.solver_stderr.is_empty() {
                let _ = write!(content, "Standard error:\n{}\n", target.solver_stderr);
            }

            content.push_str(
                "Please generate ONE improved SMT-LIB2 formula for the piece above. \
                 Put it in a single ```smt2 code block.\n",
            );
        }
    }

    messages.push(user_message(&content));
    messages
}

pub fn build_explanation_messages(
    input_content: &str,
    formula: &str,
    outcome: &SolverOutcome,
) -> Vec<Message> {
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
            _ => "N/A",
        }
    );

    vec![system_message(system), user_message(&prompt)]
}
