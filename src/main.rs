use anyhow::Result;
use clap::Parser;
use refactor_check::agent::{AgentConfig, run};
use refactor_check::llm::LlmConfig;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "refactor-check", about = "Check refactoring equivalence via SMT solving")]
struct Cli {
    /// Path to the input txt file describing the refactoring
    #[arg(short, long)]
    input: String,

    /// Path to the SMT solver binary
    #[arg(long, default_value = "z3")]
    solver_path: String,

    /// Primary LLM model identifier
    #[arg(long, default_value = "qwen/qwen3-coder:free")]
    primary_model: String,

    /// Judge (cheaper) LLM model identifier
    #[arg(long, default_value = "google/gemma-3-4b-it:free")]
    judge_model: String,

    /// API base URL
    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    api_base: String,

    /// API key (defaults to OPENROUTER_API_KEY env var)
    #[arg(long, env = "OPENROUTER_API_KEY")]
    api_key: Option<String>,
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

    let api_key = cli.api_key.ok_or_else(|| {
        anyhow::anyhow!("API key required: set --api-key or OPENROUTER_API_KEY env var")
    })?;

    let config = AgentConfig {
        llm_config: LlmConfig {
            api_key,
            api_base: cli.api_base,
            primary_model: cli.primary_model,
            judge_model: cli.judge_model,
        },
        solver_path: cli.solver_path,
    };

    run(&cli.input, config).await
}