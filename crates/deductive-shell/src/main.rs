use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use refactor_check_core::protocols::all_protocols;
use servyi_servatui::App;

#[derive(Parser)]
#[command(name = "deductive-shell", about = "TUI client for deductive-check server")]
struct Cli {
    socket_path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let app = App::builder(&cli.socket_path)
        .version("0.1.0")
        .protocol_all(all_protocols())
        .build();

    if !app.server_running() {
        anyhow::bail!("Cannot connect to server at {}. Is deductive-check running?", cli.socket_path.display());
    }

    app.run_tui().map_err(|e| anyhow::anyhow!("{e}"))
}
