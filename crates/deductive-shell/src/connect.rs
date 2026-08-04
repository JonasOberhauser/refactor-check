//! Connect-time socket discovery for `deductive-shell` — the client-side
//! "connect" resolver.
//!
//! Given a target path (a socket file, or a directory to scan, or the cwd),
//! resolve which Unix socket to connect to. Directory scans probe every socket
//! file with the `identify` handshake to recognize deductive-check servers.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::os::unix::fs::FileTypeExt;

use refactor_check_core::protocols::{identify_protocol, IdentifyResponse};
use servyi_servatui::App;

const SERVER_NAME: &str = "deductive-check";
/// Max time to wait for a single candidate socket to answer the handshake.
const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the path is a Unix socket file (follows symlinks).
fn is_socket(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// A probed socket candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    /// The server identity if the handshake succeeded.
    pub identity: Option<IdentifyResponse>,
}

/// Outcome of resolving a target path.
#[derive(Debug)]
pub enum Resolution {
    /// Exactly one matching server found, or the target was a socket path.
    Connect(PathBuf),
    /// Multiple matching servers found; the user must disambiguate.
    Ambiguous(Vec<Candidate>),
    /// No matching server found in the scanned directory.
    NoneFound { dir: PathBuf, probed: Vec<Candidate> },
}

/// Resolve which socket to connect to.
///
/// - If `target` is a directory, scan it for socket files and probe each with
///   the `identify` handshake. If exactly one is a deductive-check server,
///   connect to it; otherwise report the candidates.
/// - Otherwise (a file or socket path, existing or not), connect to it
///   directly. A missing socket is reported later by the connection check.
pub fn resolve(target: &Path) -> Resolution {
    if target.is_dir() {
        scan_dir(target)
    } else {
        Resolution::Connect(target.to_path_buf())
    }
}

fn scan_dir(dir: &Path) -> Resolution {
    let mut matches: Vec<Candidate> = Vec::new();
    let mut probed: Vec<Candidate> = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            return Resolution::NoneFound { dir: dir.to_path_buf(), probed: Vec::new() }
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // Only probe socket-type files; skip everything else cheaply.
        if !is_socket(&path) {
            continue;
        }
        match probe(&path) {
            Some(id) if id.name == SERVER_NAME => {
                let c = Candidate { path: path.clone(), identity: Some(id) };
                probed.push(c.clone());
                matches.push(c);
            }
            Some(id) => {
                probed.push(Candidate { path, identity: Some(id) });
            }
            None => {
                probed.push(Candidate { path, identity: None });
            }
        }
    }

    match matches.len() {
        1 => Resolution::Connect(matches.pop().map(|c| c.path).unwrap()),
        0 => Resolution::NoneFound { dir: dir.to_path_buf(), probed },
        _ => Resolution::Ambiguous(matches),
    }
}

/// Probe a socket with the `identify` handshake, guarded by a deadline so a
/// non-cooperative socket cannot hang startup. Returns the server identity if
/// the socket belongs to a live server speaking our protocol.
fn probe(path: &Path) -> Option<IdentifyResponse> {
    let p = path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(probe_inner(&p));
    });
    rx.recv_timeout(PROBE_DEADLINE).ok().flatten()
}

fn probe_inner(path: &Path) -> Option<IdentifyResponse> {
    let app = App::builder(path).protocol(identify_protocol()).build();
    if !app.server_running() {
        return None;
    }
    match app.run_cli_command_raw("identify", "") {
        Ok((_lines, raw)) => serde_json::from_slice::<IdentifyResponse>(&raw).ok(),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refactor_check_core::message_log::MessageLog;
    use refactor_check_core::protocols::{all_protocols, ServerState};
    use std::sync::Arc;

    /// Spawn a minimal deductive-check-compatible server (just the protocol
    /// set, including `identify`) on a socket inside a temp dir.
    fn spawn_server(socket: PathBuf) {
        let state = Arc::new(ServerState::new(Arc::new(MessageLog::new())));
        let server_socket = socket.clone();
        std::thread::spawn(move || {
            let app = App::builder(&server_socket)
                .protocol_all(all_protocols())
                .build();
            let _ = app.run_server(state);
        });
        // Wait for the socket to appear (server removes + rebinds).
        for _ in 0..100 {
            if is_socket(&socket) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("server did not bind socket in time");
    }

    #[test]
    fn resolves_single_matching_server_in_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("deductive-check-999.sock");
        spawn_server(socket.clone());

        match resolve(dir.path()) {
            Resolution::Connect(p) => assert_eq!(p, socket),
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn empty_dir_is_none_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        match resolve(dir.path()) {
            Resolution::NoneFound { .. } => {}
            other => panic!("expected NoneFound, got {other:?}"),
        }
    }

    #[test]
    fn file_target_connects_directly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("deductive-check-7.sock");
        spawn_server(socket.clone());

        // Pointing at the socket file directly skips the scan.
        match resolve(&socket) {
            Resolution::Connect(p) => assert_eq!(p, socket),
            other => panic!("expected Connect, got {other:?}"),
        }
    }
}
