//! A servatui-display layer that surfaces server status changes as a
//! dismissable banner: a background poller queries the read-only `watch`
//! protocol and publishes [`StatusSnapshot`]s into shared state. While the
//! server has threads parked in the error gate, the banner becomes a
//! RECOVERY popup with [ RETRY NOW ] / [ WAIT FOR CONTINUE ] /
//! [ ABORT VERIFICATION ] buttons (arrow keys + Enter, or clicks); Esc or
//! WAIT dismisses it until the parked state changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use servatui_display::{DisplayLayer, EventResult, LayerCtx, StackIntent};
use servyi_servatui::connection::TypedConnection;
use servyi_servatui::{BufferConsole, NoInput, SocketConnection, WidgetEntry};

use crate::protocols::{continue_protocol, exit_protocol, query_status, StatusSnapshot};

/// Shared slot the poller publishes into and the layer reads from.
pub type StatusSlot = Arc<Mutex<Option<StatusSnapshot>>>;

/// Shared queue of error lines destined for the TUI log box (drained by
/// the display every frame).
pub type LogSink = Arc<Mutex<Vec<String>>>;

/// Poll the server's `watch` protocol every 2s and publish the latest
/// snapshot into `slot`. NEW pending errors (not seen on earlier polls)
/// are immediately queued into `log_sink` so they land in the log box
/// without waiting for the user to run anything. Best-effort: unreachable
/// server leaves the last snapshot in place. Runs until process exit.
pub fn spawn_status_poller(socket: PathBuf, slot: StatusSlot, log_sink: LogSink) {
    std::thread::spawn(move || {
        let mut seen_errors: Vec<String> = Vec::new();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let Some(snap) = query_status(&socket) else {
                continue;
            };
            if snap.pending_errors != seen_errors {
                for err in &snap.pending_errors {
                    if !seen_errors.iter().any(|e| e == err) {
                        log_sink.lock().unwrap().push(format!("error: {err}"));
                    }
                }
                seen_errors = snap.pending_errors.clone();
            }
            *slot.lock().unwrap() = Some(snap);
        }
    });
}

/// Button labels on the error-gate popup's last inner row (order matters).
const GATE_BUTTONS: [&str; 3] = ["[ RETRY NOW ]", "[ WAIT FOR CONTINUE ]", "[ ABORT VERIFICATION ]"];
const GATE_WIDTH: u16 = 60;

/// Run the `continue` protocol (bump the epoch: parked threads retry).
fn send_retry(socket: &Path) -> bool {
    send_simple(socket, "continue", continue_protocol)
}

/// Run the `exit` protocol (shutdown: parked threads abort).
fn send_abort(socket: &Path) -> bool {
    send_simple(socket, "exit", exit_protocol)
}

fn send_simple(socket: &Path, name: &str, proto: impl Fn() -> servyi_servatui::Protocol) -> bool {
    let Ok(mut conn) = SocketConnection::connect(socket) else {
        return false;
    };
    if conn.send_typed(&name.to_string()).is_err() {
        return false;
    }
    proto()
        .run_client("", &mut conn, &mut BufferConsole::new(), &mut NoInput)
        .is_ok()
}

/// Banner lines for a snapshot without parked gate errors, or `None`.
fn plain_banner_lines(snap: &StatusSnapshot) -> Option<Vec<String>> {
    if !snap.pending_errors.is_empty() {
        let mut lines = vec![format!(
            " {} pending error{} — Esc dismisses, 'continue' resumes ",
            snap.pending_errors.len(),
            if snap.pending_errors.len() == 1 { "" } else { "s" },
        )];
        for err in snap.pending_errors.iter().take(3) {
            lines.push(err.to_string());
        }
        if snap.pending_errors.len() > 3 {
            lines.push(format!(" … and {} more", snap.pending_errors.len() - 3));
        }
        Some(lines)
    } else {
        snap.result.as_ref().map(|result| vec![format!(" work finished: {result} ")])
    }
}

fn in_rect(a: &Rect, col: u16, row: u16) -> bool {
    col >= a.x && col < a.x + a.width && row >= a.y && row < a.y + a.height
}

/// Screen rects of the three gate buttons on the popup's last inner row.
/// Must match what the button row renders.
fn gate_button_rects(popup: Rect) -> [Rect; 3] {
    let row = popup.y + popup.height.saturating_sub(2);
    let mut x = popup.x + 1;
    let mut rects = [Rect::default(); 3];
    for (i, label) in GATE_BUTTONS.iter().enumerate() {
        let w = label.chars().count() as u16;
        rects[i] = Rect::new(x, row, w, 1);
        x += w + 2;
    }
    rects
}

enum Banner {
    None,
    /// Top-right plain banner (pending errors / work finished).
    Plain(Vec<String>),
    /// Error-gate recovery popup with buttons.
    Gate(Vec<String>),
}

/// Display layer: top-right status banner; a recovery popup while the
/// server is parked in the error gate. Dismissed by Esc/click (plain) or
/// Esc/WAIT (gate); re-opened automatically when the state changes.
pub struct StatusLayer {
    socket: PathBuf,
    slot: StatusSlot,
    last_signature: Option<(Vec<String>, Option<String>, Vec<String>)>,
    dismissed: bool,
    /// Focused gate button (0 = retry, 1 = wait, 2 = abort).
    focus: usize,
}

impl StatusLayer {
    pub fn new(socket: impl Into<PathBuf>, slot: StatusSlot) -> Self {
        Self {
            socket: socket.into(),
            slot,
            last_signature: None,
            dismissed: false,
            focus: 0,
        }
    }

    /// The banner to show this frame, after applying dismissal/re-open
    /// logic against the latest snapshot. Idempotent between frames.
    fn current_banner(&mut self) -> Banner {
        let Some(snap) = self.slot.lock().unwrap().clone() else {
            return Banner::None;
        };
        // Content-based: a retry that drains errors and a re-park with a
        // new error must count as a CHANGE even when the counts repeat.
        let sig = (
            snap.pending_errors.clone(),
            snap.result.clone(),
            snap.parked.clone(),
        );
        if self.last_signature.as_ref() != Some(&sig) {
            self.last_signature = Some(sig);
            self.dismissed = false;
        }
        if self.dismissed {
            return Banner::None;
        }
        if !snap.parked.is_empty() {
            let mut lines = vec![format!(
                " verification paused — {} thread{} parked in the error gate: ",
                snap.parked.len(),
                if snap.parked.len() == 1 { "" } else { "s" },
            )];
            for err in snap.parked.iter().take(2) {
                lines.push(err.clone());
            }
            Banner::Gate(lines)
        } else {
            match plain_banner_lines(&snap) {
                Some(lines) => Banner::Plain(lines),
                None => Banner::None,
            }
        }
    }

    fn gate_banner_area(&self, terminal: Rect, lines: usize) -> Rect {
        let w = GATE_WIDTH.min(terminal.width.saturating_sub(2));
        let h = (lines as u16 + 3).min(terminal.height);
        Rect::new(
            terminal.x + terminal.width.saturating_sub(w),
            terminal.y,
            w,
            h,
        )
    }
}

impl DisplayLayer for StatusLayer {
    fn tab_label(&self) -> char {
        '!'
    }

    /// No taskbar button (and nothing clickable) while there is no banner —
    /// the reserved slot means the button returns at the same position as
    /// soon as something needs attention.
    fn hide_when_empty(&self) -> bool {
        true
    }

    fn on_overlay(&mut self, ctx: &mut LayerCtx, widgets: &mut Vec<WidgetEntry>) -> StackIntent {
        match self.current_banner() {
            Banner::None => {}
            Banner::Plain(lines) => {
                let w = 48.min(ctx.terminal_area.width.saturating_sub(2));
                let h = (lines.len() as u16 + 2).min(ctx.terminal_area.height);
                let area = Rect::new(
                    ctx.terminal_area.x + ctx.terminal_area.width.saturating_sub(w),
                    ctx.terminal_area.y,
                    w,
                    h,
                );
                widgets.push(WidgetEntry {
                    name: "shell.status.banner",
                    widget: Box::new(
                        Paragraph::new(lines.join("\r\n"))
                            .block(Block::default().borders(Borders::ALL)),
                    ),
                    area,
                });
            }
            Banner::Gate(lines) => {
                let n_lines = lines.len() + 1; // + button row
                let area = self.gate_banner_area(ctx.terminal_area, n_lines);
                let mut text: Vec<Line> =
                    lines.into_iter().map(Line::from).collect();
                text.push(Line::from(
                    GATE_BUTTONS
                        .iter()
                        .enumerate()
                        .flat_map(|(i, label)| {
                            let span = Span::styled(
                                label.to_string(),
                                if i == self.focus {
                                    Style::default().add_modifier(Modifier::REVERSED)
                                } else {
                                    Style::default()
                                },
                            );
                            [span, Span::raw("  ")]
                        })
                        .collect::<Vec<_>>(),
                ));
                widgets.push(WidgetEntry {
                    name: "shell.status.banner",
                    widget: Box::new(
                        Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
                    ),
                    area,
                });
            }
        }
        StackIntent::Keep
    }

    fn on_event(&mut self, ev: &Event, ctx: &LayerCtx) -> EventResult {
        match self.current_banner() {
            Banner::None => EventResult::Pass,
            Banner::Plain(_) => match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press && k.code == KeyCode::Esc => {
                    self.dismissed = true;
                    EventResult::Swallow
                }
                Event::Mouse(m) if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) => {
                    self.dismissed = true;
                    EventResult::Swallow
                }
                _ => EventResult::Pass,
            },
            Banner::Gate(_) => match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Left | KeyCode::Up => {
                        self.focus = (self.focus + GATE_BUTTONS.len() - 1) % GATE_BUTTONS.len();
                        EventResult::Swallow
                    }
                    KeyCode::Right | KeyCode::Down => {
                        self.focus = (self.focus + 1) % GATE_BUTTONS.len();
                        EventResult::Swallow
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        self.activate_button(self.focus);
                        EventResult::Swallow
                    }
                    KeyCode::Esc => {
                        // Same as WAIT: keep waiting, just hide the popup.
                        self.dismissed = true;
                        EventResult::Swallow
                    }
                    _ => EventResult::Pass,
                },
                Event::Mouse(m) if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) => {
                    let Some((_, popup)) =
                        ctx.my_widgets.iter().find(|(n, _)| *n == "shell.status.banner")
                    else {
                        return EventResult::Pass;
                    };
                    let rects = gate_button_rects(*popup);
                    for (i, rect) in rects.iter().enumerate() {
                        if in_rect(rect, m.column, m.row) {
                            self.activate_button(i);
                            break;
                        }
                    }
                    // The click is ours either way; other clicks just
                    // don't trigger anything.
                    EventResult::Swallow
                }
                _ => EventResult::Pass,
            },
        }
    }
}

impl StatusLayer {
    fn activate_button(&mut self, idx: usize) {
        match idx {
            0 => {
                let _ = send_retry(&self.socket);
                self.dismissed = true;
            }
            1 => {
                // WAIT: keep waiting for 'continue' (typed in the input
                // line); just hide the popup until the state changes.
                self.dismissed = true;
            }
            2 => {
                let _ = send_abort(&self.socket);
                self.dismissed = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use servatui_display::Display;

    pub(super) fn snap(running: bool, result: Option<&str>, errors: &[&str]) -> StatusSnapshot {
        StatusSnapshot {
            running,
            result: result.map(String::from),
            pending_errors: errors.iter().map(|s| s.to_string()).collect(),
            parked: Vec::new(),
        }
    }


    pub(super) fn frame() -> Vec<WidgetEntry> {
        let w = |name: &'static str, area: Rect| WidgetEntry {
            name,
            widget: Box::new(Paragraph::new("x")),
            area,
        };
        vec![
            w(servyi_servatui::WIDGET_LOG, Rect::new(0, 0, 80, 21)),
            w(servyi_servatui::WIDGET_INPUT, Rect::new(0, 21, 80, 3)),
        ]
    }

    pub(super) fn has_banner(frame: &[WidgetEntry]) -> Option<Rect> {
        frame.iter().find(|w| w.name == banner_name()).map(|w| w.area)
    }

    fn banner_name() -> &'static str {
        "shell.status.banner"
    }

    #[test]
    fn banner_appears_for_pending_errors_and_work_finished() {
        let slot: StatusSlot = Arc::new(Mutex::new(None));
        let mut display = Display::with_palette(vec![ratatui::style::Color::Blue]);
        display.add_layer(Box::new(StatusLayer::new("/nonexistent-status-test.sock", slot.clone())));

        // Clean running state: no banner.
        *slot.lock().unwrap() = Some(snap(true, None, &[]));
        let mut f = frame();
        display.frame(&mut f);
        assert!(!f.iter().any(|w| w.name == banner_name()));

        // Pending errors: banner in the top-right corner.
        *slot.lock().unwrap() = Some(snap(true, None, &["boom"]));
        let mut f = frame();
        display.frame(&mut f);
        let banner = f.iter().find(|w| w.name == banner_name()).expect("banner shown");
        assert_eq!(banner.area.x + banner.area.width, 80);
        assert_eq!(banner.area.y, 0);

        // Work finished: banner too.
        *slot.lock().unwrap() = Some(snap(false, Some("ok"), &[]));
        let mut f = frame();
        display.frame(&mut f);
        assert!(f.iter().any(|w| w.name == banner_name()));
    }

    #[test]
    fn esc_and_click_dismiss_and_new_errors_reopen() {
        use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};

        let slot: StatusSlot = Arc::new(Mutex::new(Some(snap(true, None, &["boom"]))));
        let mut display = Display::with_palette(vec![ratatui::style::Color::Blue]);
        display.add_layer(Box::new(StatusLayer::new("/nonexistent-status-test.sock", slot.clone())));

        let mut f = frame();
        display.frame(&mut f);
        let banner = f.iter().find(|w| w.name == banner_name()).unwrap().area;

        // Esc dismisses (keys are offered to layers regardless of widgets).
        let esc = Event::Key(crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(display.route_event(&esc));
        let mut f = frame();
        display.frame(&mut f);
        assert!(!f.iter().any(|w| w.name == banner_name()), "Esc must dismiss");

        // Unchanged snapshot stays dismissed.
        let mut f = frame();
        display.frame(&mut f);
        assert!(!f.iter().any(|w| w.name == banner_name()));

        // A changed snapshot re-opens the banner; a click inside dismisses.
        *slot.lock().unwrap() = Some(snap(true, None, &["boom", "bang"]));
        let mut f = frame();
        display.frame(&mut f);
        assert!(f.iter().any(|w| w.name == banner_name()), "new errors re-open");

        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: banner.x + 1,
            row: banner.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(display.route_event(&click));
        let mut f = frame();
        display.frame(&mut f);
        assert!(!f.iter().any(|w| w.name == banner_name()), "click must dismiss");
    }
}

#[cfg(test)]
mod taskbar_tests {
    use super::tests::{frame, snap};
    use super::*;
    use servatui_display::Display;

    fn taskbar_buttons(display: &mut Display) -> usize {
        let mut f = frame();
        display.frame(&mut f);
        f.iter().filter(|w| w.name == "display.taskbar").count()
    }

    #[test]
    fn status_button_appears_only_while_the_banner_is_shown() {
        let slot: StatusSlot = Arc::new(Mutex::new(None));
        let mut display = Display::with_palette(vec![ratatui::style::Color::Blue]);
        display.add_layer(Box::new(StatusLayer::new("/nonexistent-status-test.sock", slot.clone())));

        // Idle (running, no errors, no result): no banner, taskbar shows
        // only the builtin button.
        *slot.lock().unwrap() = Some(snap(true, None, &[]));
        assert_eq!(taskbar_buttons(&mut display), 1);

        // Pending errors: banner and the status button appear.
        *slot.lock().unwrap() = Some(snap(true, None, &["boom"]));
        assert_eq!(taskbar_buttons(&mut display), 2);

        // Dismissed: banner gone, button gone again (slot stays reserved).
        let esc = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(display.route_event(&esc));
        assert_eq!(taskbar_buttons(&mut display), 1);

        // New errors: the button is back at its reserved slot.
        *slot.lock().unwrap() = Some(snap(true, None, &["boom", "bang"]));
        assert_eq!(taskbar_buttons(&mut display), 2);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::tests::{frame as builtin_frame, has_banner};
    use super::*;
    use crate::protocols::ServerState;
    use servatui_display::Display;

    fn key(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE))
    }

    fn enter() -> Event {
        key(KeyCode::Enter)
    }

    #[test]
    fn gate_popup_buttons_retry_wait_and_abort() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gate.sock");

        let state = Arc::new(ServerState::new(Arc::new(crate::message_log::MessageLog::new())));
        let handle = servyi_servatui::ServerHandle {
            socket: socket.clone(),
            protocols: crate::protocols::all_protocols(&socket),
        };
        let state2 = state.clone();
        std::thread::spawn(move || handle.run(state2).ok());
        for _ in 0..200 {
            if SocketConnection::server_exists(&socket) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let slot: StatusSlot = Arc::new(Mutex::new(None));
        let mut display = Display::with_palette(vec![ratatui::style::Color::Blue]);
        display.add_layer(Box::new(StatusLayer::new(socket.clone(), slot.clone())));

        let refresh = |slot: &StatusSlot| {
            *slot.lock().unwrap() = query_status(&socket);
        };
        let frame = |display: &mut Display| {
            let mut f = builtin_frame();
            display.frame(&mut f);
            f
        };

        // Parked: the recovery popup appears.
        state.push_error("boom".to_string());
        state.error_gate_parked.lock().unwrap().push("z3 exploded".to_string());
        refresh(&slot);
        let f = frame(&mut display);
        assert!(has_banner(&f).is_some(), "gate popup shown");

        // Enter on the focused [ RETRY NOW ] runs the continue protocol:
        // pending errors drained (epoch bumped; the parked thread would
        // resume and clear itself).
        assert!(display.route_event(&enter()));
        assert!(
            state.pending_errors.lock().unwrap().is_empty(),
            "retry must run 'continue'"
        );
        assert!(!state.shutdown.load(std::sync::atomic::Ordering::Acquire));

        // Park again; WAIT (focus 1) does neither.
        state.push_error("again".to_string());
        state.error_gate_parked.lock().unwrap().push("z3 exploded".to_string());
        refresh(&slot);
        assert!(display.route_event(&key(KeyCode::Right))); // focus -> WAIT
        assert!(display.route_event(&enter()));
        assert_eq!(
            state.pending_errors.lock().unwrap().len(),
            1,
            "wait must not run 'continue'"
        );
        assert!(!state.shutdown.load(std::sync::atomic::Ordering::Acquire));

        // WAIT dismissed the popup; a state change re-opens it (focus is
        // preserved). ABORT (one more Right) runs the exit protocol.
        state.push_error("third".to_string());
        refresh(&slot);
        assert!(display.route_event(&key(KeyCode::Right))); // focus -> ABORT
        assert!(display.route_event(&enter()));
        assert!(
            state.shutdown.load(std::sync::atomic::Ordering::Acquire),
            "abort must run 'exit'"
        );

        let _ = std::fs::remove_file(&socket);
    }
}
