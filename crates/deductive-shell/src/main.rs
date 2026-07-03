use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use refactor_check_core::tui::TuiClient;

#[derive(Parser)]
#[command(name = "deductive-shell", about = "TUI client for deductive-check server")]
struct Cli {
    socket_path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = TuiClient::new(cli.socket_path.to_string_lossy().to_string());
    client.run()
}
