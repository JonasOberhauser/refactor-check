use anyhow::Context;

use crate::llm::{LlmConfig, Message, system_message, user_message};
use crate::piece_manager::DefaultPieceManager;
use crate::smt::SolverOutcome;

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
            "--- Formula ---\n[{} #{}]\n{}\nOutcome: {:?}, Verdict: {}\n",
            f.piece_label, f.piece_id, f.formula, f.outcome, f.verdict
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