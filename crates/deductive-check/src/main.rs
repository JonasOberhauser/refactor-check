use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use deductive_check::machine;
use deductive_check::piece_manager::DefaultDeductivePieceManager;
use refactor_check_core::llm::LlmConfig;
use refactor_check_core::smt::Z3Solver;

#[derive(Parser)]
#[command(name = "deductive-check", about = "Deductive verification of code correctness")]
struct Cli {
    #[arg(long)]
    project: String,

    #[arg(long, default_value = "z3")]
    solver_path: String,

    #[arg(long, default_value = "-in", num_args = 0.., value_delimiter = ' ')]
    solver_args: Vec<String>,

    #[arg(long, default_value_t = 60)]
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
    analyzer_model: Option<String>,

    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    api_base: String,

    #[arg(long, env = "OPENROUTER_API_KEY")]
    api_key: Option<String>,

    #[arg(long, default_value = "rust-analyzer")]
    rust_analyzer_path: String,

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
                .unwrap_or_else(|_| EnvFilter::new("deductive_check=info")),
        )
        .init();

    let cli = Cli::parse();

    let api_key = cli.api_key.unwrap_or_else(|| {
        std::env::var("OPENROUTER_API_KEY").unwrap_or_default()
    });

    let service_tier = match cli.service_tier.to_lowercase().as_str() {
        "auto" => refactor_check_core::llm::ServiceTier::Auto,
        "default" => refactor_check_core::llm::ServiceTier::Default,
        "flex" => refactor_check_core::llm::ServiceTier::Flex,
        "scale" => refactor_check_core::llm::ServiceTier::Scale,
        "priority" => refactor_check_core::llm::ServiceTier::Priority,
        other => anyhow::bail!(
            "Invalid service tier: {other}. Valid values: auto, default, flex, scale, priority"
        ),
    };

    let api_model = cli.api_model;
    let judge_model = cli.judge_model.unwrap_or_else(|| api_model.clone());

    let llm_config = LlmConfig {
        api_key,
        judge_api_key: None,
        formalizer_api_key: None,
        fixer_api_key: None,
        splitter_api_key: None,
        splitting_judge_api_key: None,
        analyzer_api_key: None,
        api_base: cli.api_base,
        formalizer_model: cli.formalizer_model.unwrap_or_else(|| api_model.clone()),
        fixer_model: cli.fixer_model.unwrap_or_else(|| api_model.clone()),
        judge_model: judge_model.clone(),
        splitting_judge_model: judge_model,
        splitter_model: cli.splitter_model.unwrap_or_else(|| api_model.clone()),
        analyzer_model: cli.analyzer_model.unwrap_or_else(|| api_model.clone()),
        stream_timeout_ms: cli.stream_timeout_ms,
        max_stream_retries: cli.max_stream_retries,
        service_tier,
    };

    let llm = refactor_check_core::llm::LlmClient::new(llm_config);
    let solver = Z3Solver::with_config(
        cli.solver_path.clone(),
        cli.solver_args,
        std::time::Duration::from_secs(cli.solver_timeout_secs),
    );

    let rust_analyzer = deductive_check::provider::CliRustAnalyzerProvider::new(cli.rust_analyzer_path);
    let git = deductive_check::provider::CliGitProvider::new();
    let filesystem = deductive_check::provider::LocalFileSystemProvider::new();
    let python = deductive_check::provider::ProcessPythonProvider::new();

    let providers = deductive_check::provider::Providers {
        llm: &llm,
        solver: &solver,
        rust_analyzer: &rust_analyzer,
        git: &git,
        filesystem: &filesystem,
        python: &python,
    };

    let pm = DefaultDeductivePieceManager::new();

    let result = machine::run(&cli.project, &providers, &pm).await?;

    println!("\n{}", "=" .repeat(60));
    println!("Deductive verification complete");
    println!("  Total pieces:  {}", result.total_pieces);
    println!("  Closed:       {}", result.closed_pieces.len());
    println!("  Unverified:   {}", result.unverified_pieces.len());
    println!("  Bug reports:  {}", result.bug_reports.len());
    println!("{}", "=" .repeat(60));

    if !result.unverified_pieces.is_empty() {
        println!("\nUnverified pieces:");
        for p in &result.unverified_pieces {
            println!("  {}:{}-{}: {}",
                p.file.display(),
                p.start_line,
                p.end_line,
                p.function_id.display_name(),
            );
        }
    }

    if !result.bug_reports.is_empty() {
        println!("\nBug reports:");
        for b in &result.bug_reports {
            println!("  {}: {}", b.file.display(), b.description);
        }
    }

    Ok(())
}