use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use deductive_check::machine;
use deductive_check::piece_manager::DefaultDeductivePieceManager;
use refactor_check_core::config_update::{AppConfig, ServiceTierArg, UpdateArgs};
use refactor_check_core::error_gate::ErrorShell;
use refactor_check_core::llm::LlmConfig;
use refactor_check_core::live_config::LiveConfig;
use refactor_check_core::smt::{SolverConfig, Z3Solver};

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

    #[arg(long, value_enum, default_value = "priority")]
    service_tier: ServiceTierArg,

    #[arg(long, default_value = "opencode")]
    agent_binary: String,

    #[arg(long, default_value = "run")]
    agent_subcommand: String,

    #[arg(long, default_value_t = true)]
    agent_skip_permissions: bool,

    #[arg(long, default_value = "opencode/deepseek-v4-flash-free")]
    agent_model: String,

    #[arg(long)]
    agent_dir: Option<String>,

    #[arg(long, default_value = "verification/result.json")]
    result_file: String,
}

fn resolve_api_key(key: Option<&str>) -> String {
    let raw = key
        .map(|s| {
            let path = std::path::Path::new(s);
            if path.is_file() {
                std::fs::read_to_string(path)
                    .unwrap_or_else(|e| {
                        eprintln!("Warning: could not read api key file {s}: {e}");
                        s.to_string()
                    })
                    .trim()
                    .to_string()
            } else {
                s.to_string()
            }
        })
        .unwrap_or_else(|| std::env::var("OPENROUTER_API_KEY").unwrap_or_default());
    raw
}

impl From<&Cli> for LlmConfig {
    fn from(cli: &Cli) -> Self {
        let api_model = cli.api_model.clone();
        let judge_model = cli.judge_model.clone().unwrap_or_else(|| api_model.clone());
        LlmConfig {
            api_key: resolve_api_key(cli.api_key.as_deref()),
            judge_api_key: None,
            formalizer_api_key: None,
            fixer_api_key: None,
            splitter_api_key: None,
            splitting_judge_api_key: None,
            analyzer_api_key: None,
            api_base: cli.api_base.clone(),
            formalizer_model: cli.formalizer_model.clone().unwrap_or_else(|| api_model.clone()),
            fixer_model: cli.fixer_model.clone().unwrap_or_else(|| api_model.clone()),
            judge_model: judge_model.clone(),
            splitting_judge_model: judge_model,
            splitter_model: cli.splitter_model.clone().unwrap_or_else(|| api_model.clone()),
            analyzer_model: cli.analyzer_model.clone().unwrap_or(api_model),
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
                .unwrap_or_else(|_| EnvFilter::new("deductive_check=info")),
        )
        .init();

    let cli = Cli::parse();

    let config_live = Arc::new(LiveConfig::new(AppConfig::from(&cli)));

    let project = cli.project.clone();
    let agent_binary = cli.agent_binary.clone();
    let agent_subcommand = cli.agent_subcommand.clone();
    let agent_model = cli.agent_model.clone();
    let agent_skip_permissions = cli.agent_skip_permissions;
    let result_file = cli.result_file.clone();

    let shell = ErrorShell::with_base_plugins()
        .with_config::<UpdateArgs, AppConfig>("set", config_live.clone())?;

    shell.run(
        move |gate| async move {
            let mut llm = refactor_check_core::llm::LlmClient::with_live_config(config_live.clone());
            let mut solver = Z3Solver::with_live_config(config_live.clone());
            if let Some(g) = &gate {
                llm = llm.with_error_gate(g.clone());
                solver = solver.with_error_gate(g.clone());
            }

            let rust_analyzer = deductive_check::provider::CliRustAnalyzerProvider::new(project.clone())?;
            let git = deductive_check::provider::CliGitProvider::new();
            let filesystem = deductive_check::provider::LocalFileSystemProvider::new();
            let python = deductive_check::provider::ProcessPythonProvider::new();

            let mut agent_args = vec![agent_subcommand];
            if agent_skip_permissions {
                agent_args.push("--dangerously-skip-permissions".to_string());
            }
            if !agent_model.is_empty() {
                agent_args.push("-m".to_string());
                agent_args.push(agent_model);
            }
            let mut agent = deductive_check::provider::CliAgentProvider::new(agent_binary, agent_args);
            if let Some(g) = &gate {
                agent = agent.with_error_gate(g.clone());
            }

            let providers = deductive_check::provider::Providers {
                llm: &llm,
                solver: &solver,
                rust_analyzer: &rust_analyzer,
                git: &git,
                filesystem: &filesystem,
                python: &python,
                agent: &agent,
            };

            let pm = DefaultDeductivePieceManager::new();

            let result = machine::run(&project, &providers, &pm).await?;

            let result_path = std::path::PathBuf::from(&result_file);
            if let Some(parent) = result_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if let Err(e) = result.save_to_file(&result_path) {
                eprintln!("Warning: failed to save result to {}: {e}", result_path.display());
            } else {
                eprintln!("Result saved to {}", result_path.display());
            }

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
        },
    )
}
