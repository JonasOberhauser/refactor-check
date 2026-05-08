use std::sync::OnceLock;

use refactor_check::agent::run_with_providers;
use refactor_check::llm::LlmConfig;
use refactor_check::smt::Z3Solver;

static API_KEY: OnceLock<Option<String>> = OnceLock::new();

fn get_api_key() -> Option<&'static str> {
    API_KEY
        .get_or_init(|| std::env::var("OPENROUTER_API_KEY").ok())
        .as_deref()
}

fn free_models_config() -> LlmConfig {
    LlmConfig {
        api_key: "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"
            .to_string(),
        judge_api_key: None,
        api_base: "https://openrouter.ai/api/v1".to_string(),
        primary_model: "qwen/qwen3-coder:free".to_string(),
        judge_model: "google/gemma-3-4b-it:free".to_string(),
        stream_timeout_ms: 8000,
        max_stream_retries: 7,
    }
}

#[test_log::test(tokio::test)]
async fn test_e2e_struct_max_equivalent() {
    let api_key = get_api_key().unwrap_or("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");

    let input = std::fs::read_to_string("struct_max_equivalent.txt")
        .expect("test input file should exist");

    let llm = refactor_check::llm::LlmClient::new(LlmConfig {
        api_key: api_key.to_string(),
        ..free_models_config()
    });

    let solver = Z3Solver::new("z3".to_string());

    let result = run_with_providers(&input, &llm, &solver)
        .await
        .expect("agent should succeed");

    eprintln!("Formulas: {}/{} reasonable", result.formulas.len(), result.formulas.len() + result.open_count);
    eprintln!("Overall equivalent: {}", result.overall_equivalent);
    for f in &result.formulas {
        eprintln!("  {:?} -> {}", f.outcome, f.verdict);
    }
    if result.open_count > 0 {
        eprintln!("Open items: {}", result.open_count);
    }

    assert!(
        result.overall_equivalent,
        "expected equivalent (UNSAT), got opposite. SAT: {}, UNSAT: {}, UNKNOWN: {}, OPEN: {}",
        result.reasonable_sat, result.reasonable_unsat, result.reasonable_unknown, result.open_count
    );
    assert!(
        result.reasonable_unsat > 0,
        "expected at least one UNSAT formula, got SAT: {}, UNSAT: {}, UNKNOWN: {}",
        result.reasonable_sat, result.reasonable_unsat, result.reasonable_unknown
    );
}

#[test_log::test(tokio::test)]
async fn test_e2e_struct_max_non_equivalent() {
    let api_key = get_api_key().unwrap_or("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");

    let input = std::fs::read_to_string("struct_max_non_equivalent.txt")
        .expect("test input file should exist");

    let llm = refactor_check::llm::LlmClient::new(LlmConfig {
        api_key: api_key.to_string(),
        ..free_models_config()
    });

    let solver = Z3Solver::new("z3".to_string());

    let result = run_with_providers(&input, &llm, &solver)
        .await
        .expect("agent should succeed");

    eprintln!("Formulas: {}/{} reasonable", result.formulas.len(), result.formulas.len() + result.open_count);
    eprintln!("Overall equivalent: {}", result.overall_equivalent);
    for f in &result.formulas {
        eprintln!("  {:?} -> {}", f.outcome, f.verdict);
    }
    if result.open_count > 0 {
        eprintln!("Open items: {}", result.open_count);
    }

    assert!(
        !result.overall_equivalent,
        "expected NOT equivalent (SAT), got equivalent. SAT: {}, UNSAT: {}, UNKNOWN: {}, OPEN: {}",
        result.reasonable_sat, result.reasonable_unsat, result.reasonable_unknown, result.open_count
    );
    assert!(
        result.reasonable_sat > 0,
        "expected at least one SAT formula, got SAT: {}, UNSAT: {}, UNKNOWN: {}",
        result.reasonable_sat, result.reasonable_unsat, result.reasonable_unknown
    );
}
