use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;

use refactor_check_core::protocols::{all_protocols, StatusSnapshot};
use refactor_check_core::status_layer::{spawn_status_poller, StatusLayer};
use servatui_display::Display;
use servyi_servatui::SocketConnection;

#[derive(Parser)]
#[command(name = "deductive-shell", about = "TUI client for deductive-check server")]
struct Cli {
    socket_path: PathBuf,
}

fn main() {
    let cli = Cli::parse();

    if !SocketConnection::server_exists(&cli.socket_path) {
        eprintln!(
            "Cannot connect to server at {}. Is deductive-check running?",
            cli.socket_path.display()
        );
        std::process::exit(1);
    }

    // Background status poller -> shared snapshot; the StatusLayer shows a
    // dismissable banner whenever the server has pending errors or finishes.
    let slot: Arc<Mutex<Option<StatusSnapshot>>> = Arc::new(Mutex::new(None));
    spawn_status_poller(cli.socket_path.clone(), slot.clone());

    let mut display = Display::new();
    display.add_layer(Box::new(StatusLayer::new(slot)));

    let protocols = all_protocols(&cli.socket_path);
    if let Err(e) = display.run(&cli.socket_path, &protocols) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
