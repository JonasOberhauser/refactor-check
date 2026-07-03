use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
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

use crate::protocol::{write_msg, ClientMsg, CommandInfo, OutKind, ServerMsg};

struct LogEntry {
    text: String,
    kind: OutKind,
}

impl OutKind {
    fn style(self) -> Style {
        match self {
            OutKind::Info => Style::default().fg(Color::DarkGray),
            OutKind::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            OutKind::Output => Style::default(),
        }
    }
}

pub struct TuiClient {
    socket_path: String,
}

impl TuiClient {
    #[must_use]
    pub fn new(socket_path: String) -> Self {
        Self { socket_path }
    }

    pub fn run(&self) -> Result<()> {
        if !io::stdin().is_terminal() {
            bail!("TUI client requires a terminal");
        }

        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| anyhow::anyhow!("cannot connect to server at {}: {e}\nIs deductive-check running?", self.socket_path))?;

        // Send/receive channels
        let (server_tx, server_rx) = mpsc::channel::<ServerMsg>();
        let commands: Arc<Mutex<Vec<CommandInfo>>> = Arc::new(Mutex::new(Vec::new()));

        // Reader thread: reads ServerMsgs from socket, forwards to channel
        let reader_stream = stream.try_clone()?;
        let reader_cmds = commands.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(reader_stream);
            for line in reader.lines() {
                match line {
                    Ok(line) if !line.is_empty() => {
                        if let Ok(msg) = serde_json::from_str::<ServerMsg>(&line) {
                            if let ServerMsg::Commands { list } = &msg {
                                *reader_cmds.lock().unwrap() = list.clone();
                            }
                            let _ = server_tx.send(msg);
                        }
                    }
                    Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        let writer = std::io::BufWriter::new(stream);
        let writer = Arc::new(Mutex::new(writer));

        // TUI state
        let mut log_entries: Vec<LogEntry> = Vec::new();
        let mut input = Input::default();
        let mut history: Vec<String> = Vec::new();
        let mut history_idx: Option<usize> = None;
        let mut log_scroll_up: u16 = 0;
        let mut finished = false;

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
                // Drain server messages
                while let Ok(msg) = server_rx.try_recv() {
                    match msg {
                        ServerMsg::Output { text, kind } => {
                            log_entries.push(LogEntry { text, kind });
                            log_scroll_up = 0;
                        }
                        ServerMsg::Error { text } => {
                            log_entries.push(LogEntry { text, kind: OutKind::Error });
                            log_scroll_up = 0;
                        }
                        ServerMsg::Status { state, message } => {
                            let label = match state {
                                crate::protocol::WorkState::Running => "running",
                                crate::protocol::WorkState::Finished => "finished",
                                crate::protocol::WorkState::Failed => "failed",
                            };
                            log_entries.push(LogEntry {
                                text: if message.is_empty() {
                                    format!("[Background work {label}]")
                                } else {
                                    format!("[Background work {label}: {message}]")
                                },
                                kind: if state == crate::protocol::WorkState::Failed {
                                    OutKind::Error
                                } else {
                                    OutKind::Info
                                },
                            });
                            if state != crate::protocol::WorkState::Running {
                                finished = true;
                            }
                            log_scroll_up = 0;
                        }
                        ServerMsg::Commands { .. } | ServerMsg::Done => {}
                    }
                }

                // Draw
                terminal.draw(|f| {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(3), Constraint::Length(3)])
                        .split(f.area());

                    let log_height_inner = chunks[0].height.saturating_sub(2) as usize;
                    let total = log_entries.len();
                    let max_scroll = total.saturating_sub(log_height_inner);
                    log_scroll_up = log_scroll_up.min(max_scroll as u16);
                    let scroll = max_scroll.saturating_sub(log_scroll_up as usize);

                    let lines: Vec<Line> = log_entries
                        .iter()
                        .map(|e| Line::from(Span::styled(&e.text, e.kind.style())))
                        .collect();

                    let title = if finished { "Output (finished)" } else { "Output (running)" };

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

                let poll_timeout = if finished { Duration::from_secs(5) } else { Duration::from_millis(300) };

                if event::poll(poll_timeout)? {
                    let ev = event::read()?;
                    if let Event::Key(key) = ev {
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

                        // Ctrl+Up/Down for history
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
                                        Some((n, a)) => (n.to_string(), a.trim().to_string()),
                                        None => (cmd.clone(), String::new()),
                                    };

                                    // Send command to server
                                    let msg = ClientMsg::Command { name, args };
                                    let mut w = writer.lock().unwrap();
                                    let _ = write_msg(&mut *w, &msg);
                                    let _ = w.flush();
                                    drop(w);

                                    // Read responses until Done
                                    while let Ok(resp) = server_rx.recv_timeout(Duration::from_secs(30)) {
                                        match resp {
                                            ServerMsg::Output { text, kind } => {
                                                log_entries.push(LogEntry { text, kind });
                                            }
                                            ServerMsg::Done => break,
                                            ServerMsg::Error { text } => {
                                                log_entries.push(LogEntry { text, kind: OutKind::Error });
                                            }
                                            ServerMsg::Status { state, .. } => {
                                                if state != crate::protocol::WorkState::Running {
                                                    finished = true;
                                                }
                                            }
                                            ServerMsg::Commands { .. } => {}
                                        }
                                    }
                                    log_scroll_up = 0;
                                    input.reset();
                                }
                            }

                            KeyCode::Tab => {
                                let current = input.value();
                                let cmds = commands.lock().unwrap();
                                if let Some(completion) = cmds
                                    .iter()
                                    .find(|c| c.name.starts_with(current) && c.name.len() > current.len())
                                {
                                    input = Input::new(completion.name.clone());
                                }
                            }

                            KeyCode::Char('c') if ctrl => {
                                input.reset();
                                history_idx = None;
                            }

                            KeyCode::Char('d') if ctrl && input.value().is_empty() => {
                                return Ok(());
                            }

                            _ => {
                                input.handle_event(&Event::Key(key));
                            }
                        }
                    }
                }
            }
        })();

        // Cleanup
        {
            let stdout = terminal.backend_mut();
            write!(stdout, "\x1b[?1007l")?;
        }
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

        tui_result
    }
}
