use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use tui_input::{backend::crossterm::EventHandler, Input};

use crate::error_gate::{ErrorGate, ShellContext, ShellPlugin};

struct LogEntry {
    text: String,
    kind: LogKind,
}

#[derive(Clone, Copy)]
enum LogKind {
    Info,
    Error,
    Output,
}

impl LogKind {
    fn style(self) -> Style {
        match self {
            LogKind::Info => Style::default().fg(Color::DarkGray),
            LogKind::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            LogKind::Output => Style::default(),
        }
    }
}

pub struct TuiShell {
    plugins: Vec<Box<dyn ShellPlugin>>,
}

impl TuiShell {
    #[must_use]
    pub fn new() -> Self {
        Self { plugins: Vec::new() }
    }

    #[must_use]
    pub fn with_base_plugins() -> Self {
        let mut shell = Self::new();
        shell.plugins.push(Box::new(crate::error_gate::ContinuePlugin));
        shell.plugins.push(Box::new(crate::error_gate::ExitPlugin));
        shell.plugins.push(Box::new(crate::error_gate::HelpPlugin));
        shell
    }

    pub fn with_plugin(mut self, plugin: Box<dyn ShellPlugin>) -> Result<Self> {
        let name = plugin.name().to_string();
        if self.plugins.iter().any(|p| p.name() == name.as_str()) {
            anyhow::bail!("plugin '{name}' already registered");
        }
        self.plugins.push(plugin);
        Ok(self)
    }

    pub fn with_config<A, C>(mut self, name: &'static str, config: Arc<crate::live_config::LiveConfig<C>>) -> Result<Self>
    where
        A: crate::config_update::ApplyTo<C>,
        C: Clone + Send + Sync + 'static,
    {
        self.plugins.push(Box::new(crate::config_update::SetPlugin::<A, C>::new(name, config)));
        Ok(self)
    }

    pub fn run<F, Fut>(self, work: F) -> Result<()>
    where
        F: FnOnce(Option<Arc<ErrorGate>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        if !io::stdin().is_terminal() {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            return rt.block_on(work(None));
        }

        let epoch = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let exit_flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<String>();
        let gate = Arc::new(ErrorGate::new(epoch.clone(), shutdown.clone(), tx));

        let bg_handle = std::thread::Builder::new()
            .name("verification".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(work(Some(gate)))
            })?;

        let plugin_infos: Vec<crate::error_gate::PluginInfo> = self
            .plugins
            .iter()
            .map(|p| crate::error_gate::PluginInfo {
                name: p.name().to_string(),
                description: p.description().to_string(),
            })
            .collect();
        let command_names: Vec<String> = plugin_infos.iter().map(|p| p.name.clone()).collect();
        let plugins = self.plugins;

        // TUI state
        let mut log_entries: Vec<LogEntry> = Vec::new();
        let mut input = Input::default();
        let mut history: Vec<String> = Vec::new();
        let mut history_idx: Option<usize> = None;
        let mut log_scroll_up: u16 = 0;
        let mut bg_done = false;
        let mut bg_handle_opt: Option<std::thread::JoinHandle<Result<()>>> = Some(bg_handle);

        // Terminal setup
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        write!(stdout, "\x1b[?1007h")?;
        stdout.flush()?;

        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let tui_result = (|| {
            loop {
                // Drain errors from background thread
                while let Ok(error) = rx.try_recv() {
                    log_entries.push(LogEntry { text: error, kind: LogKind::Error });
                    log_scroll_up = 0;
                }

                // Check background completion
                if !bg_done {
                    if let Some(handle) = bg_handle_opt.as_ref() {
                        if handle.is_finished() {
                            bg_done = true;
                            let handle = bg_handle_opt.take().unwrap();
                            match handle.join() {
                                Ok(Ok(())) => {
                                    log_entries.push(LogEntry {
                                        text: "[Background work finished]".to_string(),
                                        kind: LogKind::Info,
                                    });
                                }
                                Ok(Err(e)) => {
                                    log_entries.push(LogEntry {
                                        text: format!("[Background work error: {e:#}]"),
                                        kind: LogKind::Error,
                                    });
                                }
                                Err(_) => {
                                    log_entries.push(LogEntry {
                                        text: "[Background thread panicked]".to_string(),
                                        kind: LogKind::Error,
                                    });
                                }
                            }
                            log_scroll_up = 0;
                        }
                    }
                }

                // Clamp scroll
                let total = log_entries.len();

                // Draw
                terminal.draw(|f| {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(3), Constraint::Length(3)])
                        .split(f.area());

                    let log_height_inner = chunks[0].height.saturating_sub(2) as usize;
                    let max_scroll = total.saturating_sub(log_height_inner);
                    let scroll = max_scroll.saturating_sub(log_scroll_up as usize);

                    let lines: Vec<Line> = log_entries
                        .iter()
                        .map(|e| Line::from(Span::styled(&e.text, e.kind.style())))
                        .collect();

                    let title = if bg_done { "Output (finished)" } else { "Output (running)" };

                    f.render_widget(
                        Paragraph::new(lines)
                            .scroll((scroll as u16, 0))
                            .block(Block::default().borders(Borders::ALL).title(title))
                            .wrap(Wrap { trim: false }),
                        chunks[0],
                    );

                    let input_text = input.value();
                    let cursor = input.visual_cursor();
                    let prefix = "$ ";

                    let input_line = Line::from(vec![
                        Span::styled(prefix, Style::default().fg(Color::Cyan)),
                        Span::raw(input_text),
                    ]);

                    f.render_widget(
                        Paragraph::new(input_line)
                            .block(Block::default().borders(Borders::ALL).title("Command")),
                        chunks[1],
                    );

                    let input_y = chunks[1].y + 1;
                    let input_x = chunks[1].x + 1 + prefix.len() as u16 + cursor as u16;
                    f.set_cursor_position((input_x, input_y));
                })?;

                // Poll for events
                let poll_timeout = if bg_done { Duration::from_secs(5) } else { Duration::from_millis(300) };

                if event::poll(poll_timeout)? {
                    let ev = event::read()?;
                    if let Event::Key(key) = ev {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

                        // Ctrl+Up/Down for history (check before plain Up/Down scroll)
                        if ctrl && key.code == KeyCode::Up {
                            if !history.is_empty() {
                                history_idx = match history_idx {
                                    None => Some(history.len() - 1),
                                    Some(0) => Some(0),
                                    Some(idx) => Some(idx - 1),
                                };
                                if let Some(idx) = history_idx {
                                    input = Input::new(history[idx].clone());
                                }
                            }
                            continue;
                        }
                        if ctrl && key.code == KeyCode::Down {
                            if let Some(idx) = history_idx {
                                if idx + 1 < history.len() {
                                    history_idx = Some(idx + 1);
                                    input = Input::new(history[idx + 1].clone());
                                } else {
                                    history_idx = None;
                                    input.reset();
                                }
                            }
                            continue;
                        }

                        match key.code {
                            KeyCode::Up => { log_scroll_up = log_scroll_up.saturating_add(1); }
                            KeyCode::Down => { log_scroll_up = log_scroll_up.saturating_sub(1); }
                            KeyCode::PageUp => { log_scroll_up = log_scroll_up.saturating_add(10); }
                            KeyCode::PageDown => { log_scroll_up = log_scroll_up.saturating_sub(10); }

                            KeyCode::Enter => {
                                let cmd = input.value().trim().to_string();
                                if !cmd.is_empty() {
                                    history.push(cmd.clone());
                                    history_idx = None;

                                    let (name, args) = match cmd.split_once(char::is_whitespace) {
                                        Some((n, a)) => (n, a.trim()),
                                        None => (cmd.as_str(), ""),
                                    };

                                    let ctx = ShellContext::new(
                                        &epoch,
                                        &exit_flag,
                                        &plugin_infos,
                                    );

                                    match plugins.iter().find(|p| p.name() == name) {
                                        Some(plugin) => {
                                            let msg = plugin.handle(args, &ctx);
                                            if !msg.is_empty() {
                                                for line in msg.lines() {
                                                    log_entries.push(LogEntry {
                                                        text: line.to_string(),
                                                        kind: LogKind::Output,
                                                    });
                                                }
                                            }
                                        }
                                        None => {
                                            log_entries.push(LogEntry {
                                                text: format!(
                                                    "[unknown command: '{cmd}' — type 'help']"
                                                ),
                                                kind: LogKind::Info,
                                            });
                                        }
                                    }

                                    input.reset();
                                    log_scroll_up = 0;
                                }
                            }

                            KeyCode::Tab => {
                                let current = input.value();
                                if let Some(completion) = command_names
                                    .iter()
                                    .find(|name| name.starts_with(current) && name.len() > current.len())
                                {
                                    input = Input::new(completion.clone());
                                }
                            }

                            KeyCode::Char('c') if ctrl => {
                                input.reset();
                                history_idx = None;
                            }

                            KeyCode::Char('d') if ctrl && input.value().is_empty() => {
                                shutdown.store(true, Ordering::Release);
                                epoch.fetch_add(1, Ordering::Release);
                                drop(bg_handle_opt.take());
                                return Ok(());
                            }

                            _ => {
                                input.handle_event(&Event::Key(key));
                            }
                        }
                    }
                }

                if exit_flag.load(Ordering::Acquire) {
                    shutdown.store(true, Ordering::Release);
                    epoch.fetch_add(1, Ordering::Release);
                    drop(bg_handle_opt.take());
                    return Ok(());
                }
            }
        })();

        // Cleanup terminal
        {
            let stdout = terminal.backend_mut();
            write!(stdout, "\x1b[?1007l")?;
        }
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

        // Wait for bg thread to finish
        if let Some(handle) = bg_handle_opt.take() {
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = done_tx.send(handle.join());
            });
            match done_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    eprintln!("[background work error: {e:#}]");
                }
                _ => {}
            }
        }

        tui_result
    }
}

impl Default for TuiShell {
    fn default() -> Self {
        Self::new()
    }
}
