use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use servyi_servatui::{Plugin, Protocol, ShellAction};
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

pub fn show_protocol() -> Protocol {
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

/// Identity returned by the `identify` handshake. Used by clients (e.g.
/// `deductive-shell`'s connect resolver) to recognize a matching server among
/// candidate socket files.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IdentifyResponse {
    pub name: String,
    pub version: String,
    pub pid: u32,
}

/// Handshake protocol: a client probes a socket with `identify` to learn
/// whether it speaks the deductive-check protocol and which process owns it.
pub fn identify_protocol() -> Protocol {
    Plugin::new("identify", "Server identity handshake")
        .parse(|_args: &str| Ok(()))
        .client(|req: (), _out, _input| Ok(req))
        .server_ctx(|_req: (), _ctx: &ServerState| {
            Ok(IdentifyResponse {
                name: "deductive-check".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
            })
        })
        .client(|resp: IdentifyResponse, out, _input| {
            out.print_line(&format!(
                "[{} v{} pid={}]",
                resp.name, resp.version, resp.pid
            ));
            Ok(())
        })
        .finalize(|| Ok(ShellAction::Continue))
}

pub fn all_protocols() -> Vec<Protocol> {
    vec![
        continue_protocol(),
        show_protocol(),
        status_protocol(),
        exit_protocol(),
        identify_protocol(),
    ]
}

// ── Show helpers (same logic as the old ShowPlugin) ──

fn show_tree(log: &MessageLog) -> String {
    let keys = log.keys();
    if keys.is_empty() {
        return "No pieces recorded yet".to_string();
    }
    let mut sorted = keys;
    sorted.sort_by(|a, b| {
        let sa: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
        let sb: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
        sa.cmp(&sb)
    });
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
