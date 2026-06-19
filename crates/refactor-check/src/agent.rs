use anyhow::Context;
use std::sync::Arc;

use refactor_check_core::config_update::AppConfig;
use crate::error_gate::ErrorGate;
use crate::llm::{Message, system_message, user_message};
use crate::live_config::LiveConfig;
use crate::piece_manager::DefaultPieceManager;
use crate::smt::SolverOutcome;

pub use crate::smt::DEFAULT_SOLVER_TIMEOUT_SECS;

pub async fn run(
    input_file: &str,
    config: Arc<LiveConfig<AppConfig>>,
    error_gate: Option<Arc<ErrorGate>>,
) -> anyhow::Result<()> {
    let input_content = std::fs::read_to_string(input_file)
        .with_context(|| format!("Failed to read input file: {input_file}"))?;

    let mut llm = crate::llm::LlmClient::with_live_config(config.clone());
    let mut solver = crate::smt::Z3Solver::with_live_config(config);
    if let Some(gate) = &error_gate {
        llm = llm.with_error_gate(gate.clone());
        solver = solver.with_error_gate(gate.clone());
    }
    let pm = DefaultPieceManager::new();

    let result = crate::machine::run(&input_content, &llm, &solver, &pm).await?;
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
            "--- Formula ---\n[{}]\n{}\nOutcome: {:?}, Verdict: {}\n",
            f.piece_label, f.formula, f.outcome, f.verdict
        );
        if let Some(ref explanation) = f.explanation {
            println!("--- Explanation ---\n{explanation}\n");
        }
    }
    Ok(())
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