use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use refactor_check::agent::{DEFAULT_SOLVER_TIMEOUT_SECS, run};
use refactor_check::config_update::{AppConfig, ServiceTierArg, UpdateArgs};
use refactor_check::error_gate::ErrorShell;
use refactor_check::llm::LlmConfig;
use refactor_check::live_config::LiveConfig;
use refactor_check::smt::SolverConfig;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "refactor-check", about = "Check refactoring equivalence via SMT solving")]
struct Cli {
    #[arg(short, long)]
    input: String,

    #[arg(long, default_value = "z3")]
    solver_path: String,

    #[arg(long, default_value = "-in", num_args = 0.., value_delimiter = ' ')]
    solver_args: Vec<String>,

    #[arg(long, default_value_t = DEFAULT_SOLVER_TIMEOUT_SECS)]
    solver_timeout_secs: u64,

    #[arg(long, default_value = "openrouter/free")]
    api_model: String,

    #[arg(long)]
    splitter_model: Option<String>,

    #[arg(long)]
    formalizer_model: Option<String>,

    #[arg(long)]
    fixer_model: Option<String>,

    #[arg(long)]
    judge_model: Option<String>,

    #[arg(long)]
    splitting_judge_model: Option<String>,

    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    api_base: String,

    #[arg(long, env = "OPENROUTER_API_KEY")]
    api_key: Option<String>,

    #[arg(long, env = "JUDGE_API_KEY")]
    judge_api_key: Option<String>,

    #[arg(long, env = "FORMALIZER_API_KEY")]
    formalizer_api_key: Option<String>,

    #[arg(long, env = "FIXER_API_KEY")]
    fixer_api_key: Option<String>,

    #[arg(long, env = "SPLITTER_API_KEY")]
    splitter_api_key: Option<String>,

    #[arg(long, env = "SPLITTING_JUDGE_API_KEY")]
    splitting_judge_api_key: Option<String>,

    #[arg(long, default_value = "3000")]
    stream_timeout_ms: u64,

    #[arg(long, default_value = "5")]
    max_stream_retries: u32,

    #[arg(long, value_enum, default_value = "priority")]
    service_tier: ServiceTierArg,
}

impl From<&Cli> for LlmConfig {
    fn from(cli: &Cli) -> Self {
        let api_model = cli.api_model.clone();
        let judge_model = cli.judge_model.clone().unwrap_or_else(|| api_model.clone());
        LlmConfig {
            api_key: cli.api_key.clone().unwrap_or_default(),
            judge_api_key: cli.judge_api_key.clone(),
            formalizer_api_key: cli.formalizer_api_key.clone(),
            fixer_api_key: cli.fixer_api_key.clone(),
            splitter_api_key: cli.splitter_api_key.clone(),
            splitting_judge_api_key: cli.splitting_judge_api_key.clone(),
            analyzer_api_key: None,
            api_base: cli.api_base.clone(),
            splitter_model: cli.splitter_model.clone().unwrap_or_else(|| api_model.clone()),
            formalizer_model: cli.formalizer_model.clone().unwrap_or_else(|| api_model.clone()),
            fixer_model: cli.fixer_model.clone().unwrap_or_else(|| api_model.clone()),
            judge_model: judge_model.clone(),
            splitting_judge_model: cli.splitting_judge_model.clone().unwrap_or(judge_model),
            analyzer_model: api_model,
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("refactor_check=info")),
        )
        .init();

    let cli = Cli::parse();

    let config_live = Arc::new(LiveConfig::new(AppConfig::from(&cli)));

    let input = cli.input.clone();

    let shell = ErrorShell::with_base_plugins()
        .with_config::<UpdateArgs, AppConfig>("set", config_live.clone())?;

    shell.run(
        move |gate| async move {
            run(&input, config_live, gate).await
        },
    )
}
