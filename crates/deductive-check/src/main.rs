use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use refactor_check_core::protocols::{all_protocols, ServerState};
use refactor_check_core::config_update::{AppConfig, ServiceTierArg};
use refactor_check_core::live_config::LiveConfig;
use refactor_check_core::llm::LlmConfig;
use refactor_check_core::message_log::{MessageLog, MessageLogLayer};
use refactor_check_core::smt::{SolverConfig, Z3Solver};
use servyi_servatui::App;

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

    #[arg(long, default_value = "")]
    api_key: Option<String>,

    #[arg(long)]
    judge_api_key: Option<String>,

    #[arg(long)]
    formalizer_api_key: Option<String>,

    #[arg(long)]
    fixer_api_key: Option<String>,

    #[arg(long)]
    splitter_api_key: Option<String>,

    #[arg(long)]
    splitting_judge_api_key: Option<String>,

    #[arg(long, default_value = "")]
    api_base: String,

    #[arg(long)]
    api_model: String,

    #[arg(long)]
    judge_model: Option<String>,

    #[arg(long)]
    formalizer_model: Option<String>,

    #[arg(long)]
    fixer_model: Option<String>,

    #[arg(long)]
    splitter_model: Option<String>,

    #[arg(long)]
    analyzer_model: Option<String>,

    #[arg(long, default_value_t = 3000)]
    stream_timeout_ms: u64,

    #[arg(long, default_value_t = 5)]
    max_stream_retries: u32,

    #[arg(long, default_value = "opencode")]
    agent_binary: String,

    #[arg(long, default_value = "run")]
    agent_subcommand: String,

    #[arg(long, default_value = "opencode/big-pickle")]
    agent_model: String,

    #[arg(long, default_value_t = true)]
    agent_skip_permissions: bool,

    #[arg(long)]
    result_file: Option<String>,

    #[arg(long)]
    log: Option<String>,

    #[arg(long, default_value = "auto")]
    service_tier: ServiceTierArg,
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.starts_with('~') || s.starts_with('.')
}

fn resolve_api_key(key: Option<&str>) -> Result<String> {
    match key {
        Some(s) if looks_like_path(s) => {
            let expanded = shellexpand::full(s)
                .map_err(|e| anyhow::anyhow!("cannot expand path {s}: {e}"))?;
            let path = std::path::Path::new(&*expanded);
            if path.is_file() {
                let content = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("cannot read api key file {}: {e}", path.display()))?;
                Ok(content.trim().to_string())
            } else {
                Err(anyhow::anyhow!(
                    "api key file not found: {}\n\
                     Create the file with your API key, or pass the key directly with --api-key <KEY>",
                    path.display()
                ))
            }
        }
        Some(s) => Ok(s.to_string()),
        None => Ok(std::env::var("OPENROUTER_API_KEY").unwrap_or_default()),
    }
}

fn resolve_binary(name: &str) -> String {
    if name.contains('/') {
        return name.to_string();
    }
    let Ok(path) = std::env::var("PATH") else { return name.to_string(); };
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(dir).join(name);
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    name.to_string()
}

impl From<&Cli> for LlmConfig {
    fn from(cli: &Cli) -> Self {
        let api_model = cli.api_model.clone();
        let judge_model = cli.judge_model.clone().unwrap_or_else(|| api_model.clone());
        LlmConfig {
            api_key: cli.api_key.clone().unwrap_or_default(),
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

fn parse_log_spec(spec: &str) -> (String, bool) {
    if let Some(path) = spec.strip_prefix("ansi:") {
        (path.to_string(), true)
    } else {
        (spec.to_string(), false)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log = Arc::new(MessageLog::new());

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("deductive_check=info"));

    if let Some(log_spec) = &cli.log {
        let (path, force_ansi) = parse_log_spec(log_spec);
        let file = std::fs::File::create(&path)?;
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file)
            .with_ansi(force_ansi);
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(file_layer)
            .with(MessageLogLayer::new(log.clone()))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(MessageLogLayer::new(log.clone()))
            .init();
    }

    let config_live = Arc::new(LiveConfig::new(AppConfig::from(&cli)));

    let project = cli.project.clone();
    let agent_binary = resolve_binary(&cli.agent_binary);
    let agent_subcommand = cli.agent_subcommand.clone();
    let agent_model = cli.agent_model.clone();
    let agent_skip_permissions = cli.agent_skip_permissions;
    let result_file = cli.result_file.clone().unwrap_or_else(|| "result.json".to_string());
    let api_key_raw = cli.api_key.clone();

    // Server state shared with all protocol handlers
    let server_state = Arc::new(ServerState::new(log.clone()));

    // Spawn verification work in background
    let bg_state = server_state.clone();
    let bg_config = config_live.clone();
    let bg_log = log.clone();
    let bg_shutdown = server_state.shutdown.clone();
    let bg_epoch = server_state.epoch.clone();
    let bg_project = project.clone();

    let (err_tx, err_rx) = std::sync::mpsc::channel::<String>();

    let bg_handle = std::thread::Builder::new()
        .name("verification".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = err_tx.send(format!("Failed to create runtime: {e}"));
                    return;
                }
            };

            // Create error gate for recoverable errors in LLM/SMT/Agent
            let (gate_tx, gate_rx) = std::sync::mpsc::channel();
            let gate = std::sync::Arc::new(
                refactor_check_core::error_gate::ErrorGate::new(
                    bg_epoch.clone(),
                    bg_shutdown.clone(),
                    gate_tx,
                )
            );
            // Forward gate errors to server state for status/continue protocols
            {
                let gate_state = bg_state.clone();
                std::thread::spawn(move || {
                    while let Ok(e) = gate_rx.recv() {
                        gate_state.push_error(e);
                    }
                });
            }

            let result: anyhow::Result<()> = rt.block_on(async move {
                info!("starting verification worker");

                let api_key = loop {
                    info!("resolving api key");
                    match resolve_api_key(api_key_raw.as_deref()) {
                        Ok(key) => {
                            info!("api key resolved");
                            break key;
                        }
                        Err(e) => {
                            let _ = err_tx.send(format!("{e:#}"));
                            // Wait for epoch change (user types continue)
                            let my_epoch = bg_epoch.load(Ordering::Acquire);
                            loop {
                                if bg_shutdown.load(Ordering::Acquire) {
                                    return Ok(());
                                }
                                if bg_epoch.load(Ordering::Acquire) != my_epoch {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            }
                        }
                    }
                };

                bg_config.update(|cfg| cfg.llm.api_key = api_key.clone());

                info!("creating llm client");
                let llm = refactor_check_core::llm::LlmClient::with_live_config(bg_config.clone())
                    .with_error_gate(gate.clone());
                info!("creating solver");
                let solver = Z3Solver::with_live_config(bg_config.clone())
                    .with_error_gate(gate.clone());

                info!(project = %bg_project, "loading rust-analyzer workspace");
                let rust_analyzer = deductive_check::provider::CliRustAnalyzerProvider::new(bg_project.clone())?;
                info!("rust-analyzer workspace loaded");
                let git = deductive_check::provider::CliGitProvider::new(PathBuf::from(&bg_project));
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
                let agent = deductive_check::provider::CliAgentProvider::new(agent_binary, agent_args)
                    .with_error_gate(gate.clone());

                let providers = deductive_check::provider::Providers {
                    llm: &llm,
                    solver: &solver,
                    rust_analyzer: &rust_analyzer,
                    git: &git,
                    filesystem: &filesystem,
                    python: &python,
                    agent: &agent,
                };

                let pm = deductive_check::piece_manager::DefaultDeductivePieceManager::new(bg_log.clone());

                let result = deductive_check::machine::run(&bg_project, &providers, &pm).await?;

                let result_path = std::path::PathBuf::from(&result_file);
                if let Some(parent) = result_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if let Err(e) = result.save_to_file(&result_path) {
                    warn!("failed to save result to {}: {e}", result_path.display());
                } else {
                    println!("Result saved to {}", result_path.display());
                }

                println!("\n{}", "=".repeat(60));
                println!("Deductive verification complete");
                println!("  Total pieces:  {}", result.total_pieces);
                println!("  Closed:       {}", result.closed_pieces.len());
                println!("  Unverified:   {}", result.unverified_pieces.len());
                println!("  Bug reports:  {}", result.bug_reports.len());
                println!("{}", "=".repeat(60));

                Ok(())
            });

            bg_state.work_finished.store(true, Ordering::Release);
            match &result {
                Ok(()) => {
                    *bg_state.work_result.lock().unwrap() = None;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    *bg_state.work_result.lock().unwrap() = Some(msg.clone());
                    bg_state.push_error(msg);
                }
            }
        })?;

    // Error forwarding thread: drains err_rx into server state
    let fwd_state = server_state.clone();
    std::thread::spawn(move || {
        while let Ok(error) = err_rx.recv() {
            fwd_state.push_error(error);
        }
    });

    // Build the server
    let socket_path = format!("/tmp/deductive-check-{}.sock", std::process::id());
    info!(socket = %socket_path, "server listening");
    println!("Server listening on {socket_path}");
    println!("Connect with: deductive-shell {socket_path}");

    let app = App::builder(&socket_path)
        .version("0.1.0")
        .protocol_all(all_protocols(&socket_path))
        .build();

    // Run server (blocks until shutdown)
    app.run_server(server_state.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Shutdown: signal bg thread
    server_state.shutdown.store(true, Ordering::Release);
    server_state.epoch.fetch_add(1, Ordering::Release);

    // Wait for bg thread
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(bg_handle.join());
    });
    match done_rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(())) => {
            if let Some(err) = server_state.work_result.lock().unwrap().take() {
                println!("[background work error: {err}]");
            }
        }
        Ok(Err(_)) => {
            println!("[background thread panicked]");
        }
        Err(_) => {
            std::process::exit(0);
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
