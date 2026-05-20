use anyhow::Context;
use std::fmt::Write;

use crate::llm::{LlmConfig, Message, system_message, user_message};
use crate::smt::SolverOutcome;
use crate::states::{OpenItem, VerifiedPiece};

pub use crate::smt::DEFAULT_SOLVER_TIMEOUT_SECS;

pub struct AgentConfig {
    pub llm_config: LlmConfig,
    pub solver_path: String,
    pub solver_args: Vec<String>,
    pub solver_timeout_secs: u64,
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

    let result = crate::machine::run(&input_content, &llm, &solver).await?;
    println!(
        "=== Overall Equivalent ===\n{}\n",
        result.overall_equivalent
    );
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
            "--- Formula ---\n[{} #{}]\n{}\nOutcome: {:?}, Verdict: {}\n",
            f.piece_label, f.piece_id, f.formula, f.outcome, f.verdict
        );
        if let Some(ref explanation) = f.explanation {
            println!("--- Explanation ---\n{explanation}\n");
        }
    }
    Ok(())
}

pub fn build_generation_messages(
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
         - If any formula is satisfiable, the overall refactoring is NOT equivalent.",
    ));

    let mut content = format!("Here is the refactoring description:\n\n{input_content}\n\n");

    if !verified.is_empty() {
        content.push_str(
            "Pieces that have already been verified — you do NOT need to recheck these:\n\n",
        );
        for (i, piece) in verified.iter().enumerate() {
            let _ = write!(
                content,
                "Piece {} ({}):\n[{}] {}\n",
                i + 1,
                match &piece.outcome {
                    SolverOutcome::Sat => "SAT — NOT EQUIVALENT",
                    SolverOutcome::Unsat => "UNSAT — equivalent",
                    SolverOutcome::Unknown => "UNKNOWN — inconclusive",
                    SolverOutcome::Error(_) => unreachable!(),
                },
                piece.piece.label,
                piece.formula,
            );
        }
        content.push('\n');
    }

    if !open.is_empty() {
        content.push_str("Pieces that still need work:\n\n");
        for (i, item) in open.iter().enumerate() {
            let _ = write!(
                content,
                "Open piece {}:\nLabel: {}\nFormula:\n{}\nIssue: {}\n",
                i + 1,
                item.piece.label,
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
            "Generate SMT-LIB2 formula(s) checking equivalence. Start with one formula covering the whole refactoring. \
             If the solver times out, split the code into smaller analogous pieces (e.g., loop bodies, helper functions, \
             conditionals) and generate one formula per piece.\n",
        );
    } else {
        content.push_str(
            "Please generate new or improved formulas for the unverified pieces above. \
             You can also add new pieces if you think some behavior has not been checked yet.\n",
        );
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