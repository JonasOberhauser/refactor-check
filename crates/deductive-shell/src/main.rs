use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;

use refactor_check_core::protocols::{all_protocols, resolve_active_server, StatusSnapshot};
use refactor_check_core::status_layer::{spawn_status_poller, StatusLayer};
use servatui_display::Display;

#[derive(Parser)]
#[command(
    name = "deductive-shell",
    about = "TUI client for deductive-check server. Pass the server's socket path, or a folder to scan for an active server."
)]
struct Cli {
    /// Server socket path, or a folder containing one.
    socket_path: PathBuf,
}

fn main() {
    let cli = Cli::parse();

    // A folder scans for live sockets and handshakes each until one of OUR
    // servers answers; a direct socket path is used as-is.
    let Some(socket) = resolve_active_server(&cli.socket_path) else {
        eprintln!(
            "No active deductive-check server found at {}. Is deductive-check running?",
            cli.socket_path.display()
        );
        std::process::exit(1);
    };
    if socket != cli.socket_path {
        eprintln!("deductive-shell: connecting to {}", socket.display());
    }

    // Background status poller -> shared snapshot; the StatusLayer shows a
    // dismissable banner whenever the server has pending errors or finishes.
    let slot: Arc<Mutex<Option<StatusSnapshot>>> = Arc::new(Mutex::new(None));
    spawn_status_poller(socket.clone(), slot.clone());

    let mut display = Display::new();
    display.add_layer(Box::new(StatusLayer::new(socket.clone(), slot)));

    let protocols = all_protocols(&socket);
    if let Err(e) = display.run(&socket, &protocols) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
