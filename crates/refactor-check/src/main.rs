use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use refactor_check::agent::{DEFAULT_SOLVER_TIMEOUT_SECS, run};
use refactor_check::config_update::{AppConfig, ServiceTierArg};
use refactor_check::llm::LlmConfig;
use refactor_check::live_config::LiveConfig;
use refactor_check::smt::SolverConfig;

#[derive(Parser)]
#[command(name = "refactor-check", about = "Check refactoring equivalence via SMT solving")]
struct Cli {
    /// Input file containing before/after code
    #[arg(long)]
    input: String,

    /// API endpoint base URL
    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    api_base: String,

    /// API key (or path to file containing key)
    #[arg(long, env = "OPENROUTER_API_KEY")]
    api_key: Option<String>,

    /// Primary model
    #[arg(long)]
    primary_model: String,

    /// Judge model (defaults to primary)
    #[arg(long)]
    judge_model: Option<String>,

    /// Solver binary path
    #[arg(long, default_value = "z3")]
    solver_path: String,

    /// Solver arguments
    #[arg(long, default_value = "-in", num_args = 0.., value_delimiter = ' ')]
    solver_args: Vec<String>,

    /// Solver timeout in seconds
    #[arg(long, default_value_t = DEFAULT_SOLVER_TIMEOUT_SECS)]
    solver_timeout_secs: u64,

    /// Stream timeout in milliseconds
    #[arg(long, default_value_t = 3000)]
    stream_timeout_ms: u64,

    /// Max stream retries
    #[arg(long, default_value_t = 5)]
    max_stream_retries: u32,

    /// Service tier
    #[arg(long, default_value = "auto")]
    service_tier: ServiceTierArg,
}

impl From<&Cli> for LlmConfig {
    fn from(cli: &Cli) -> Self {
        LlmConfig {
            api_key: cli.api_key.clone().unwrap_or_default(),
            judge_api_key: None,
            formalizer_api_key: None,
            fixer_api_key: None,
            splitter_api_key: None,
            splitting_judge_api_key: None,
            analyzer_api_key: None,
            api_base: cli.api_base.clone(),
            formalizer_model: cli.primary_model.clone(),
            fixer_model: cli.primary_model.clone(),
            judge_model: cli.judge_model.clone().unwrap_or_else(|| cli.primary_model.clone()),
            splitting_judge_model: cli.judge_model.clone().unwrap_or_else(|| cli.primary_model.clone()),
            splitter_model: cli.primary_model.clone(),
            analyzer_model: cli.primary_model.clone(),
            stream_timeout_ms: cli.stream_timeout_ms,
            max_stream_retries: cli.max_stream_retries,
            service_tier: cli.service_tier.clone().into(),
        }
    }
}

impl From<&Cli> for SolverConfig {
    fn from(cli: &Cli) -> Self {
        SolverConfig {
            solver_path: cli.solver_path.clone(),
            solver_args: cli.solver_args.clone(),
            timeout_secs: cli.solver_timeout_secs,
        }
    }
}

impl From<&Cli> for AppConfig {
    fn from(cli: &Cli) -> Self {
        AppConfig {
            llm: LlmConfig::from(cli),
            solver: SolverConfig::from(cli),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_live = Arc::new(LiveConfig::new(AppConfig::from(&cli)));
    let input = cli.input.clone();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(&input, config_live, None))?;
    Ok(())
}
