use std::sync::OnceLock;

use refactor_check::llm::{LlmClient, LlmConfig, ServiceTier};
use refactor_check::machine;
use refactor_check::provider::AgentResult;
use refactor_check::smt::Z3Solver;

// free key: no credits
const FREE_API_KEY: &str = "***REDACTED***";

static API_KEY: OnceLock<String> = OnceLock::new();

fn get_api_key() -> &'static str {
    API_KEY
        .get_or_init(|| {
            std::env::var("OPENROUTER_API_KEY").unwrap_or_else(|_| FREE_API_KEY.to_string())
        })
        .as_str()
}

fn free_models_config(api_key: &str) -> LlmConfig {
    LlmConfig {
        api_key: api_key.to_string(),
        judge_api_key: None,
        formalizer_api_key: None,
        fixer_api_key: None,
        api_base: "https://openrouter.ai/api/v1".to_string(),
        formalizer_model: "openrouter/free".to_string(),
        fixer_model: "openrouter/free".to_string(),
        judge_model: "openrouter/free".to_string(),
        stream_timeout_ms: 120000,
        max_stream_retries: 2,
        service_tier: ServiceTier::Priority,
    }
}

async fn check_equivalent(input: &str) -> AgentResult {
    let api_key = get_api_key();
    let llm = LlmClient::new(free_models_config(api_key));
    let solver = Z3Solver::new("z3".to_string());
    machine::run(input, &llm, &solver).await.expect("agent should succeed")
}

fn print_result(label: &str, result: &AgentResult) {
    eprintln!(
        "[{label}] Equivalent={}, Formulas:{}/{} SAT:{} UNSAT:{} UNK:{} OPEN:{}",
        result.overall_equivalent,
        result.formulas.len(),
        result.formulas.len() + result.open_count,
        result.reasonable_sat,
        result.reasonable_unsat,
        result.reasonable_unknown,
        result.open_count,
    );
    for f in &result.formulas {
        eprintln!("  {:?} -> {}", f.outcome, f.verdict);
    }
}

#[test_log::test(tokio::test)]
async fn test_e2e_struct_max_equivalent() {
    let input = std::fs::read_to_string("struct_max_equivalent.txt")
        .expect("test input file should exist");
    let result = check_equivalent(&input).await;
    print_result("eq", &result);

    assert!(
        result.overall_equivalent,
        "expected equivalent (UNSAT), SAT:{} UNSAT:{} UNK:{} OPEN:{}",
        result.reasonable_sat, result.reasonable_unsat,
        result.reasonable_unknown, result.open_count
    );
    assert!(
        result.reasonable_unsat > 0,
        "expected at least one UNSAT, SAT:{} UNSAT:{} UNK:{}",
        result.reasonable_sat, result.reasonable_unsat,
        result.reasonable_unknown
    );
}

#[test_log::test(tokio::test)]
async fn test_e2e_struct_max_non_equivalent() {
    let input = std::fs::read_to_string("struct_max_non_equivalent.txt")
        .expect("test input file should exist");
    let result = check_equivalent(&input).await;
    print_result("neq", &result);

    assert!(
        !result.overall_equivalent,
        "expected NOT equivalent (SAT), SAT:{} UNSAT:{} UNK:{} OPEN:{}",
        result.reasonable_sat, result.reasonable_unsat,
        result.reasonable_unknown, result.open_count
    );
    assert!(
        result.reasonable_sat > 0,
        "expected at least one SAT, SAT:{} UNSAT:{} UNK:{}",
        result.reasonable_sat, result.reasonable_unsat,
        result.reasonable_unknown
    );
}
