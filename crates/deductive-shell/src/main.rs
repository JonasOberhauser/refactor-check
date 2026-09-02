use std::path::PathBuf;

use clap::Parser;

use refactor_check_core::protocols::all_protocols;
use servyi_servatui::App;

#[derive(Parser)]
#[command(name = "deductive-shell", about = "TUI client for deductive-check server")]
struct Cli {
    socket_path: PathBuf,
}

fn main() {
    let cli = Cli::parse();

    let app = App::builder(&cli.socket_path)
        .version("0.1.0")
        .protocol_all(all_protocols(&cli.socket_path))
        .build();

    if !app.server_running() {
        eprintln!("Cannot connect to server at {}. Is deductive-check running?", cli.socket_path.display());
        std::process::exit(1);
    }

    if let Err(e) = app.run_tui() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
