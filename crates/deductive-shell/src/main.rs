mod connect;

use std::path::PathBuf;

use clap::Parser;

use refactor_check_core::protocols::all_protocols;
use servyi_servatui::App;

#[derive(Parser)]
#[command(name = "deductive-shell", about = "TUI client for deductive-check server")]
struct Cli {
    /// A socket file to connect to directly, or a directory to scan for
    /// deductive-check server sockets. If omitted, the current working
    /// directory is scanned.
    path: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    let target = cli
        .path
        .unwrap_or_else(|| std::env::current_dir().expect("could not determine current directory"));

    match connect::resolve(&target) {
        connect::Resolution::Connect(socket) => {
            run_shell(&socket);
        }
        connect::Resolution::Ambiguous(candidates) => {
            eprintln!("Multiple deductive-check servers found:");
            for c in &candidates {
                let pid = c.identity.as_ref().map(|i| i.pid).unwrap_or(0);
                eprintln!("  {} (pid {pid})", c.path.display());
            }
            eprintln!("Connect directly: deductive-shell <socket-path>");
            std::process::exit(1);
        }
        connect::Resolution::NoneFound { dir, probed } => {
            eprintln!("No deductive-check server found in {}", dir.display());
            if !probed.is_empty() {
                eprintln!("Sockets probed but not matching:");
                for c in &probed {
                    eprintln!("  {}", c.path.display());
                }
            }
            std::process::exit(1);
        }
    }
}

fn run_shell(socket: &PathBuf) {
    let app = App::builder(socket)
        .version("0.1.0")
        .protocol_all(all_protocols())
        .build();

    if !app.server_running() {
        eprintln!(
            "Cannot connect to server at {}. Is deductive-check running?",
            socket.display()
        );
        std::process::exit(1);
    }

    if let Err(e) = app.run_tui() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
