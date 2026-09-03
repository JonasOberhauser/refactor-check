use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use servyi_servatui::{BufferConsole, NoInput, Plugin, Protocol, ShellAction, SocketConnection, TypedConnection};
use crate::message_log::{MessageLog, LogEntry, ancestors, is_valid_ctx_id};

/// Shared server state passed to all server_ctx steps.
pub struct ServerState {
    pub message_log: Arc<MessageLog>,
    pub epoch: Arc<AtomicU64>,
    pub shutdown: Arc<AtomicBool>,
    pub pending_errors: Mutex<Vec<String>>,
    pub work_finished: AtomicBool,
    pub work_result: Mutex<Option<String>>,
}

impl ServerState {
    pub fn new(message_log: Arc<MessageLog>) -> Self {
        Self {
            message_log,
            epoch: Arc::new(AtomicU64::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            pending_errors: Mutex::new(Vec::new()),
            work_finished: AtomicBool::new(false),
            work_result: Mutex::new(None),
        }
    }

    pub fn push_error(&self, error: String) {
        self.pending_errors.lock().unwrap().push(error);
    }

    pub fn drain_errors(&self) -> Vec<String> {
        let mut errors = self.pending_errors.lock().unwrap();
        std::mem::take(&mut *errors)
    }
}

// ── Protocol types ──

#[derive(Serialize, Deserialize)]
struct ContinueRequest {}

#[derive(Serialize, Deserialize)]
struct ContinueResponse {
    old_epoch: u64,
    new_epoch: u64,
    pending_errors: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct ShowRequest {
    piece_id: String,
}

#[derive(Serialize, Deserialize)]
struct ShowResponse {
    output: String,
}

#[derive(Serialize, Deserialize)]
struct StatusRequest {}

#[derive(Serialize, Deserialize)]
struct StatusResponse {
    running: bool,
    result: Option<String>,
    pending_errors: Vec<String>,
}

// ── Protocol builders ──

/// Cap on tab-completion suggestions so Tab cycling stays usable even for
/// large message logs.
const MAX_SUGGESTIONS: usize = 50;

/// Compare dotted piece ids numerically ("1.2" < "1.10" < "2").
fn cmp_dotted(a: &str, b: &str) -> std::cmp::Ordering {
    let sa: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let sb: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    sa.cmp(&sb)
}

fn sort_piece_ids(ids: &mut [String]) {
    ids.sort_by(|a, b| cmp_dotted(a, b));
}

/// A point-in-time view of the server's work status, as surfaced by the
/// `status` protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusSnapshot {
    pub running: bool,
    pub result: Option<String>,
    pub pending_errors: Vec<String>,
}

/// Ask the server for its work status over the socket. `None` when the
/// server is unreachable or the conversation fails — status polling is
/// best-effort.
pub fn query_status(socket: &Path) -> Option<StatusSnapshot> {
    let Ok(mut conn) = SocketConnection::connect(socket) else {
        return None;
    };
    if conn.send_typed(&"watch".to_string()).is_err() {
        return None;
    }
    let mut console = BufferConsole::new();
    let mut input = NoInput;
    match watch_protocol().run_client("", &mut conn, &mut console, &mut input) {
        Ok(raw) => {
            // The last server response is the serialized StatusResponse.
            let resp: serde_json::Result<StatusResponse> = serde_json::from_slice(&raw);
            match resp {
                Ok(r) => Some(StatusSnapshot {
                    running: r.running,
                    result: r.result,
                    pending_errors: r.pending_errors,
                }),
                Err(_) => None,
            }
        }
        Err(_) => None,
    }
}

/// Whether the server on this socket is one of OURS: handshake by running
/// the `pieces` protocol — a foreign servatui server answers
/// `Unknown command: pieces` as an error, ours succeeds (even with zero
/// pieces). A short read timeout keeps foreign daemons that never answer
/// from blocking the probe.
pub fn server_matches(socket: &Path) -> bool {
    let Ok(mut conn) = SocketConnection::connect(socket) else {
        return false;
    };
    // The probe connection is dropped afterwards, so a lingering timeout
    // setting cannot affect later connections.
    let _ = conn
        .stream
        .set_read_timeout(Some(std::time::Duration::from_millis(1000)));
    if conn.send_typed(&"pieces".to_string()).is_err() {
        return false;
    }
    pieces_protocol()
        .run_client("", &mut conn, &mut BufferConsole::new(), &mut NoInput)
        .is_ok()
}

/// Resolve what the user passed to the shell: a socket file is used
/// directly (if it handshakes); a FOLDER is scanned for live unix sockets
/// and the first one whose server handshakes wins — newest first, so with
/// several running servers the most recent one is preferred.
pub fn resolve_active_server(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return if server_matches(path) { Some(path.to_path_buf()) } else { None };
    }

    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(path)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let p = entry.path();
            let m = std::fs::metadata(&p).ok()?;
            use std::os::unix::fs::FileTypeExt as _;
            if !m.file_type().is_socket() {
                return None;
            }
            Some((m.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH), p))
        })
        .collect();
    candidates.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // newest first
    candidates
        .into_iter()
        .map(|(_, p)| p)
        .find(|p| server_matches(p))
}

/// Ask the server for all piece ids over the socket. Returns an empty list
/// whenever the server is unreachable or the conversation fails — completion
/// is best-effort and must never break the input line.
fn query_piece_ids(socket: &Path) -> Vec<String> {
    let Ok(mut conn) = SocketConnection::connect(socket) else {
        return Vec::new();
    };
    if conn.send_typed(&"pieces".to_string()).is_err() {
        return Vec::new();
    }
    let mut console = BufferConsole::new();
    let mut input = NoInput;
    match pieces_protocol().run_client("", &mut conn, &mut console, &mut input) {
        Ok(_) => console.lines,
        Err(_) => Vec::new(),
    }
}

/// Full-line `show <id>` suggestions for the confirmed input, filtered by
/// the typed prefix and capped at [`MAX_SUGGESTIONS`].
fn piece_id_suggestions(input: &str, ids: &[String]) -> Vec<String> {
    let Some(rest) = input.strip_prefix("show") else {
        return Vec::new();
    };
    let arg = rest.trim_start();
    let mut matching: Vec<String> = ids.iter().filter(|id| id.starts_with(arg)).cloned().collect();
    sort_piece_ids(&mut matching);
    matching
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|id| format!("show {id}"))
        .collect()
}

fn show_completions(socket: &Path, input: &str) -> Vec<String> {
    piece_id_suggestions(input, &query_piece_ids(socket))
}

/// `pieces` — list all piece ids in the message log, numerically sorted.
/// Used by `show`'s tab completion (which polls the server) and available
/// as a plain command.
pub fn pieces_protocol() -> Protocol {
    Plugin::new("pieces", "List all piece ids")
        .parse(|_args: &str| Ok(()))
        .client(|req: (), _out, _input| Ok(req))
        .server_ctx(|_req: (), ctx: &ServerState| {
            let mut ids = ctx.message_log.keys();
            sort_piece_ids(&mut ids);
            Ok(ids)
        })
        .client(|ids: Vec<String>, out, _input| {
            for id in &ids {
                out.print_line(id);
            }
            Ok(())
        })
        .finalize(|| Ok(ShellAction::Continue))
}

pub fn continue_protocol() -> Protocol {
    Plugin::new("continue", "Resume blocked tasks after error")
        .parse(|_args: &str| Ok(ContinueRequest {}))
        .client(|req: ContinueRequest, _out, _input| Ok(req))
        .server_ctx(|_req: ContinueRequest, ctx: &ServerState| {
            let old = ctx.epoch.fetch_add(1, Ordering::Release);
            let errors = ctx.drain_errors();
            Ok(ContinueResponse {
                old_epoch: old,
                new_epoch: old + 1,
                pending_errors: errors,
            })
        })
        .client(|resp: ContinueResponse, out, _input| {
            out.print_line(&format!("[epoch {} -> {}]", resp.old_epoch, resp.new_epoch));
            for err in &resp.pending_errors {
                out.print_error(err);
            }
            Ok(())
        })
        .finalize(|| Ok(ShellAction::Continue))
}

pub fn show_protocol(socket: impl AsRef<Path>) -> Protocol {
    let socket: PathBuf = socket.as_ref().to_path_buf();
    Plugin::new("show", "Show message history for a piece id (e.g. 'show 1.2.3'), or list all with no args")
        .parse(|args: &str| Ok(ShowRequest { piece_id: args.trim().to_string() }))
        .client(|req: ShowRequest, _out, _input| Ok(req))
        .server_ctx(|req: ShowRequest, ctx: &ServerState| {
            let output = if req.piece_id.is_empty() {
                show_tree(&ctx.message_log)
            } else {
                show_piece(&ctx.message_log, &req.piece_id)
            };
            Ok(ShowResponse { output })
        })
        .client(|resp: ShowResponse, out, _input| {
            for line in resp.output.lines() {
                out.print_line(line);
            }
            Ok(())
        })
        .finalize(|| Ok(ShellAction::Continue))
        // Tab completion polls the server's message log over the socket
        // (see `query_piece_ids`); unreachable server => no suggestions.
        .complete(move |input: &str| show_completions(&socket, input))
}

/// `watch` — read-only status for pollers: identical to `status` except it
/// never drains the pending errors, so background watchers cannot steal
/// them from the interactive `status`/`continue` commands.
pub fn watch_protocol() -> Protocol {
    Plugin::new("watch", "Read-only status (does not drain pending errors)")
        .parse(|_args: &str| Ok(StatusRequest {}))
        .client(|req: StatusRequest, _out, _input| Ok(req))
        .server_ctx(|_req: StatusRequest, ctx: &ServerState| {
            let pending = ctx.pending_errors.lock().unwrap().clone();
            Ok(StatusResponse {
                running: !ctx.work_finished.load(Ordering::Acquire),
                result: ctx.work_result.lock().unwrap().clone(),
                pending_errors: pending,
            })
        })
        .client(|resp: StatusResponse, out, _input| {
            if resp.running {
                out.print_line("[work running]");
            } else {
                match &resp.result {
                    Some(msg) => out.print_line(&format!("[work finished: {msg}]")),
                    None => out.print_line("[work finished]"),
                }
            }
            for err in &resp.pending_errors {
                out.print_error(err);
            }
            Ok(())
        })
        .finalize(|| Ok(ShellAction::Continue))
}

pub fn status_protocol() -> Protocol {
    Plugin::new("status", "Show work status and pending errors")
        .parse(|_args: &str| Ok(StatusRequest {}))
        .client(|req: StatusRequest, _out, _input| Ok(req))
        .server_ctx(|_req: StatusRequest, ctx: &ServerState| {
            let errors = ctx.drain_errors();
            let running = !ctx.work_finished.load(Ordering::Acquire);
            let result = ctx.work_result.lock().unwrap().clone();
            Ok(StatusResponse {
                running,
                result,
                pending_errors: errors,
            })
        })
        .client(|resp: StatusResponse, out, _input| {
            if resp.running {
                out.print_line("[work running]");
            } else {
                match &resp.result {
                    Some(msg) => out.print_line(&format!("[work finished: {msg}]")),
                    None => out.print_line("[work finished]"),
                }
            }
            for err in &resp.pending_errors {
                out.print_error(err);
            }
            Ok(())
        })
        .finalize(|| Ok(ShellAction::Continue))
}

pub fn exit_protocol() -> Protocol {
    Plugin::new("exit", "Quit the server")
        .parse(|_args: &str| Ok(()))
        .client(|req: (), _out, _input| Ok(req))
        .server_ctx(|_req: (), ctx: &ServerState| {
            ctx.shutdown.store(true, Ordering::Release);
            ctx.epoch.fetch_add(1, Ordering::Release);
            Ok(())
        })
        .client(|_: (), out, _input| {
            out.print_line("[shutting down]");
            Ok(())
        })
        .finalize(|| Ok(ShellAction::Exit))
}

pub fn all_protocols(socket: impl AsRef<Path>) -> Vec<Protocol> {
    vec![
        continue_protocol(),
        show_protocol(socket),
        status_protocol(),
        watch_protocol(),
        exit_protocol(),
        pieces_protocol(),
    ]
}

// ── Show helpers (same logic as the old ShowPlugin) ──

fn show_tree(log: &MessageLog) -> String {
    let keys = log.keys();
    if keys.is_empty() {
        return "No pieces recorded yet".to_string();
    }
    let mut sorted = keys;
    sort_piece_ids(&mut sorted);
    let max_id_len = sorted
        .iter()
        .map(|k| k.len() + k.matches('.').count() * 2)
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for key in &sorted {
        let depth = key.matches('.').count();
        let indent = depth * 2;
        let status = log.get_status(key).unwrap_or("-".to_string());
        let visual_len = key.len() + indent;
        let padding = max_id_len.saturating_sub(visual_len) + 2;
        for _ in 0..indent {
            out.push_str("  ");
        }
        out.push_str(key);
        for _ in 0..padding {
            out.push(' ');
        }
        out.push_str(&status);
        out.push('\n');
    }
    out
}

fn show_piece(log: &MessageLog, ctx_id: &str) -> String {
    if !is_valid_ctx_id(ctx_id) {
        return format!("Invalid piece id: '{ctx_id}' — expected dotted number like 1.2.3");
    }

    if !log.has(ctx_id) {
        return format!("No messages found for {ctx_id}");
    }

    let ancestor_ids = ancestors(ctx_id);
    let mut all_entries: Vec<(String, LogEntry)> = Vec::new();
    for ancestor in &ancestor_ids {
        for entry in log.get(ancestor) {
            all_entries.push((ancestor.clone(), entry));
        }
    }

    all_entries.sort_by_key(|(_, e)| e.seq);

    let mut out = String::new();
    let mut prev_ancestor = "";
    for (ancestor, entry) in &all_entries {
        if ancestor != prev_ancestor {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("--- {} ---\n", ancestor));
            prev_ancestor = ancestor;
        }
        let level_str = match entry.level {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARN ",
            tracing::Level::INFO => "INFO ",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "TRACE",
        };
        out.push_str(level_str);
        out.push(' ');
        out.push_str(&entry.message);
        for (name, value) in &entry.fields {
            out.push_str(&format!(" {name}={value}"));
        }
        out.push('\n');
    }

    if out.is_empty() {
        format!("No messages found for {ctx_id}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_filter_by_prefix_and_sort_numerically() {
        let ids: Vec<String> = ["2", "1.2", "1", "10", "1.10"]
            .iter().map(|s| s.to_string()).collect();
        // Dotted-numeric order: 1 < 1.2 < 1.10 < 2 < 10
        assert_eq!(
            piece_id_suggestions("show ", &ids),
            ["show 1", "show 1.2", "show 1.10", "show 2", "show 10"]
        );
        assert_eq!(piece_id_suggestions("show 1", &ids), ["show 1", "show 1.2", "show 1.10", "show 10"]);
        assert_eq!(piece_id_suggestions("show 1.", &ids), ["show 1.2", "show 1.10"]);
        assert_eq!(piece_id_suggestions("show 2", &ids), ["show 2"]);
        assert!(piece_id_suggestions("show 3", &ids).is_empty());
        // Only our own command ever reaches this completer.
        assert!(piece_id_suggestions("status ", &ids).is_empty());
    }

    #[test]
    fn suggestions_are_capped() {
        let ids: Vec<String> = (0..(MAX_SUGGESTIONS as u64 + 10)).map(|i| i.to_string()).collect();
        assert_eq!(piece_id_suggestions("show ", &ids).len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn watch_queries_status_without_draining_pending_errors() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("watch.sock");

        let state = Arc::new(ServerState::new(Arc::new(MessageLog::new())));
        state.push_error("boom".to_string());
        let handle = servyi_servatui::ServerHandle {
            socket: socket.clone(),
            protocols: all_protocols(&socket),
        };
        std::thread::spawn(move || handle.run(state).ok());

        for _ in 0..100 {
            if SocketConnection::server_exists(&socket) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // The read-only watch sees the pending error …
        let snap = query_status(&socket).expect("watch must succeed");
        assert!(snap.running);
        assert_eq!(snap.pending_errors, ["boom"]);
        // … repeatedly, without draining it (the interactive `status`
        // command still sees it afterwards).
        let snap = query_status(&socket).expect("watch must succeed again");
        assert_eq!(snap.pending_errors, ["boom"]);

        let mut console = BufferConsole::new();
        let mut input = NoInput;
        let mut conn = SocketConnection::connect(&socket).unwrap();
        conn.send_typed(&"status".to_string()).unwrap();
        status_protocol().run_client("", &mut conn, &mut console, &mut input).unwrap();
        assert!(
            console.lines.iter().any(|l| l.contains("boom")),
            "interactive status must still see the error: {:?}",
            console.lines
        );

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn pieces_completion_queries_live_server() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("comp.sock");

        let log = Arc::new(MessageLog::new());
        for (id, seq) in [("2", 3), ("1.2", 2), ("1", 1)] {
            log.push(
                id.to_string(),
                LogEntry {
                    seq,
                    level: tracing::Level::INFO,
                    target: "test".to_string(),
                    message: "m".to_string(),
                    fields: vec![],
                },
            );
        }
        let state = Arc::new(ServerState::new(log));
        let handle = servyi_servatui::ServerHandle {
            socket: socket.clone(),
            protocols: all_protocols(&socket),
        };
        std::thread::spawn(move || handle.run(state).ok());

        for _ in 0..100 {
            if SocketConnection::server_exists(&socket) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(query_piece_ids(&socket), ["1", "1.2", "2"]);
        assert_eq!(show_completions(&socket, "show 1."), ["show 1.2"]);
        assert_eq!(show_completions(&socket, "show 1"), ["show 1", "show 1.2"]);
        assert!(show_completions(&socket, "show 9").is_empty());

        let _ = std::fs::remove_file(&socket);
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn wait_online(socket: &Path) {
        for _ in 0..200 {
            if SocketConnection::server_exists(socket) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("server never came online: {}", socket.display());
    }

    fn spawn_server(socket: &Path, protocols: Vec<Protocol>) {
        let state = Arc::new(ServerState::new(Arc::new(MessageLog::new())));
        let handle = servyi_servatui::ServerHandle {
            socket: socket.to_path_buf(),
            protocols,
        };
        std::thread::spawn(move || handle.run(state).ok());
        wait_online(socket);
    }

    fn foreign_protocol() -> Protocol {
        // A perfectly valid servatui server — just not OURS (no `pieces`).
        Plugin::new("echo", "foreign server")
            .parse(|_args: &str| Ok(()))
            .client(|req: (), _out, _input| Ok(req))
            .server(|_: ()| Ok(()))
            .client(|_: (), _out, _input| Ok(()))
            .finalize(|| Ok(ShellAction::Continue))
    }

    #[test]
    fn handshake_distinguishes_our_servers_from_foreign_ones() {
        let dir = tempfile::tempdir().unwrap();

        let ours = dir.path().join("ours.sock");
        spawn_server(&ours, all_protocols(&ours));
        assert!(server_matches(&ours), "our server must handshake");

        let foreign = dir.path().join("foreign.sock");
        spawn_server(&foreign, vec![foreign_protocol()]);
        assert!(!server_matches(&foreign), "foreign server must not match");

        let _ = std::fs::remove_file(&ours);
        let _ = std::fs::remove_file(&foreign);
    }

    #[test]
    fn resolve_scans_folder_and_skips_newer_foreign_socket() {
        let dir = tempfile::tempdir().unwrap();

        // Ours first (older mtime), the foreign one second (newer, tried
        // first by the resolver) — proving the handshake decides, not the
        // ordering.
        let ours = dir.path().join("deductive-check-1.sock");
        spawn_server(&ours, all_protocols(&ours));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let foreign = dir.path().join("something-else.sock");
        spawn_server(&foreign, vec![foreign_protocol()]);

        let resolved = resolve_active_server(dir.path());
        assert_eq!(resolved.as_deref(), Some(ours.as_path()));

        // A folder with only foreign sockets, an empty folder, and a
        // nonexistent path all resolve to nothing.
        let other = tempfile::tempdir().unwrap();
        let lonely = other.path().join("lonely.sock");
        spawn_server(&lonely, vec![foreign_protocol()]);
        assert_eq!(resolve_active_server(other.path()), None);
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(resolve_active_server(empty.path()), None);
        assert_eq!(resolve_active_server(Path::new("/nonexistent-dir-xyz")), None);

        let _ = std::fs::remove_file(&ours);
        let _ = std::fs::remove_file(&foreign);
    }

    #[test]
    fn resolve_accepts_a_direct_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("direct.sock");
        spawn_server(&ours, all_protocols(&ours));
        assert_eq!(resolve_active_server(&ours).as_deref(), Some(ours.as_path()));
        let _ = std::fs::remove_file(&ours);
    }
}
