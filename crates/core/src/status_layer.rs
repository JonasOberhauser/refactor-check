//! A servatui-display layer that surfaces server status changes as a
//! dismissable banner: a background poller queries the read-only `watch`
//! protocol and publishes [`StatusSnapshot`]s into shared state; the layer
//! shows a top-right banner whenever there are pending errors or the work
//! finished, and re-opens it when the snapshot changes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};
use servatui_display::{DisplayLayer, EventResult, LayerCtx, StackIntent};
use servyi_servatui::WidgetEntry;

use crate::protocols::{query_status, StatusSnapshot};

/// Shared slot the poller publishes into and the layer reads from.
pub type StatusSlot = Arc<Mutex<Option<StatusSnapshot>>>;

/// Poll the server's `watch` protocol every `interval` and publish the
/// latest snapshot into `slot`. Best-effort: unreachable server leaves the
/// last snapshot in place. The thread runs until the process exits.
pub fn spawn_status_poller(socket: PathBuf, slot: StatusSlot) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if let Some(snap) = query_status(&socket) {
            *slot.lock().unwrap() = Some(snap);
        }
    });
}

/// Banner lines for a snapshot, or `None` when nothing to show.
fn banner_lines(snap: &StatusSnapshot) -> Option<Vec<String>> {
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

/// Signature of "what the banner is about" — a change re-opens a dismissed
/// banner.
fn signature(snap: &StatusSnapshot) -> Option<(usize, bool)> {
    banner_lines(snap).map(|_| (snap.pending_errors.len(), snap.result.is_some()))
}

fn banner_area(terminal: Rect, lines: usize) -> Rect {
    let w = 48.min(terminal.width.saturating_sub(2));
    let h = (lines as u16 + 2).min(terminal.height);
    Rect::new(
        terminal.x + terminal.width.saturating_sub(w),
        terminal.y,
        w,
        h,
    )
}

/// Display layer: top-right status banner, dismissed by Esc or a click,
/// re-opened automatically when the poller publishes a changed snapshot.
pub struct StatusLayer {
    slot: StatusSlot,
    last_signature: Option<(usize, bool)>,
    dismissed: bool,
}

impl StatusLayer {
    pub fn new(slot: StatusSlot) -> Self {
        Self { slot, last_signature: None, dismissed: false }
    }

    /// The banner to show this frame, after applying dismissal/re-open
    /// logic against the latest snapshot.
    fn current_banner(&mut self) -> Option<Vec<String>> {
        let snap = self.slot.lock().unwrap().clone()?;
        let sig = signature(&snap);
        if sig != self.last_signature {
            self.last_signature = sig;
            self.dismissed = false;
        }
        if self.dismissed {
            return None;
        }
        banner_lines(&snap)
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
        let Some(lines) = self.current_banner() else {
            return StackIntent::Keep;
        };
        let area = banner_area(ctx.terminal_area, lines.len());
        widgets.push(WidgetEntry {
            name: "shell.status.banner",
            widget: Box::new(
                Paragraph::new(lines.join("\r\n")).block(Block::default().borders(Borders::ALL)),
            ),
            area,
        });
        StackIntent::Keep
    }

    fn on_event(&mut self, ev: &Event, _ctx: &LayerCtx) -> EventResult {
        // Compute visibility the same way on_overlay does.
        let visible = {
            let snap = self.slot.lock().unwrap().clone();
            snap.as_ref().and_then(signature).is_some() && !self.dismissed
        };
        if !visible {
            return EventResult::Pass;
        }
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press && k.code == KeyCode::Esc => {
                self.dismissed = true;
                EventResult::Swallow
            }
            // Clicks land inside the banner's own widget area (the display
            // router already hit-tested), so any offered press dismisses.
            Event::Mouse(m) if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) => {
                self.dismissed = true;
                EventResult::Swallow
            }
            _ => EventResult::Pass,
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

    fn banner_name() -> &'static str {
        "shell.status.banner"
    }

    #[test]
    fn banner_appears_for_pending_errors_and_work_finished() {
        let slot: StatusSlot = Arc::new(Mutex::new(None));
        let mut display = Display::with_palette(vec![ratatui::style::Color::Blue]);
        display.add_layer(Box::new(StatusLayer::new(slot.clone())));

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
        display.add_layer(Box::new(StatusLayer::new(slot.clone())));

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
        display.add_layer(Box::new(StatusLayer::new(slot.clone())));

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
