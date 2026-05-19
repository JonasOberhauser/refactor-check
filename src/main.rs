use anyhow::Result;
use clap::Parser;
use refactor_check::agent::{AgentConfig, DEFAULT_SOLVER_TIMEOUT_SECS, run};
use refactor_check::llm::{LlmConfig, ServiceTier};
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

    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    api_base: String,

    #[arg(long, env = "OPENROUTER_API_KEY", default_value = "***REDACTED***")]  // free key: no credits
    api_key: Option<String>,

    #[arg(long, env = "JUDGE_API_KEY")]
    judge_api_key: Option<String>,

    #[arg(long, env = "FORMALIZER_API_KEY")]
    formalizer_api_key: Option<String>,

    #[arg(long, env = "FIXER_API_KEY")]
    fixer_api_key: Option<String>,

    #[arg(long, env = "SPLITTER_API_KEY")]
    splitter_api_key: Option<String>,

    #[arg(long, default_value = "3000")]
    stream_timeout_ms: u64,

    #[arg(long, default_value = "5")]
    max_stream_retries: u32,

    #[arg(long, default_value = "priority")]
    service_tier: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("refactor_check=info")),
        )
        .init();

    let cli = Cli::parse();

    let api_key = cli.api_key.expect("api_key has a default value");

    let service_tier = match cli.service_tier.to_lowercase().as_str() {
        "auto" => ServiceTier::Auto,
        "default" => ServiceTier::Default,
        "flex" => ServiceTier::Flex,
        "scale" => ServiceTier::Scale,
        "priority" => ServiceTier::Priority,
        other => anyhow::bail!(
            "Invalid service tier: {other}. Valid values: auto, default, flex, scale, priority"
        ),
    };

    let api_model = cli.api_model;

    let config = AgentConfig {
        llm_config: LlmConfig {
            api_key,
            judge_api_key: cli.judge_api_key,
            formalizer_api_key: cli.formalizer_api_key,
            fixer_api_key: cli.fixer_api_key,
            splitter_api_key: cli.splitter_api_key,
            api_base: cli.api_base,
            splitter_model: cli.splitter_model.unwrap_or_else(|| api_model.clone()),
            formalizer_model: cli.formalizer_model.unwrap_or_else(|| api_model.clone()),
            fixer_model: cli.fixer_model.unwrap_or_else(|| api_model.clone()),
            judge_model: cli.judge_model.unwrap_or_else(|| api_model.clone()),
            stream_timeout_ms: cli.stream_timeout_ms,
            max_stream_retries: cli.max_stream_retries,
            service_tier,
        },
        solver_path: cli.solver_path,
        solver_args: cli.solver_args,
        solver_timeout_secs: cli.solver_timeout_secs,
    };

    run(&cli.input, config).await
}