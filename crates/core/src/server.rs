use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::error_gate::{ErrorGate, ShellContext, ShellPlugin};
use crate::protocol::{
    write_msg, ClientMsg, CommandInfo, OutKind, ServerMsg, WorkState,
};

struct SharedState {
    clients: Mutex<Vec<UnixStream>>,
    work_state: Mutex<WorkState>,
    work_message: Mutex<String>,
}

pub struct Server {
    plugins: Vec<Box<dyn ShellPlugin>>,
    socket_path: String,
}

impl Server {
    #[must_use]
    pub fn new(socket_path: String) -> Self {
        Self {
            plugins: Vec::new(),
            socket_path,
        }
    }

    pub fn with_base_plugins(mut self) -> Self {
        self.plugins.push(Box::new(crate::error_gate::ContinuePlugin));
        self.plugins.push(Box::new(crate::error_gate::ExitPlugin));
        self.plugins.push(Box::new(crate::error_gate::HelpPlugin));
        self
    }

    pub fn with_plugin(mut self, plugin: Box<dyn ShellPlugin>) -> Result<Self> {
        let name = plugin.name().to_string();
        if self.plugins.iter().any(|p| p.name() == name.as_str()) {
            bail!("plugin '{name}' already registered");
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

    fn command_infos(&self) -> Vec<CommandInfo> {
        self.plugins
            .iter()
            .map(|p| CommandInfo {
                name: p.name().to_string(),
                description: p.description().to_string(),
            })
            .collect()
    }

    pub fn run<F, Fut>(self, work: F) -> Result<()>
    where
        F: FnOnce(Option<Arc<ErrorGate>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let _ = std::fs::remove_file(&self.socket_path);
        let listener = UnixListener::bind(&self.socket_path)?;

        let epoch = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let exit_flag = Arc::new(AtomicBool::new(false));
        let (err_tx, err_rx) = mpsc::channel::<String>();
        let gate = Arc::new(ErrorGate::new(
            epoch.clone(),
            shutdown.clone(),
            err_tx,
        ));

        let shared = Arc::new(SharedState {
            clients: Mutex::new(Vec::new()),
            work_state: Mutex::new(WorkState::Running),
            work_message: Mutex::new(String::new()),
        });

        // Background verification thread
        let shared_bg = shared.clone();
        let bg_handle = thread::Builder::new()
            .name("verification".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                let result = rt.block_on(work(Some(gate)));
                let (state, msg) = match &result {
                    Ok(()) => (WorkState::Finished, String::new()),
                    Err(e) => (WorkState::Failed, format!("{e:#}")),
                };
                *shared_bg.work_state.lock().unwrap() = state;
                *shared_bg.work_message.lock().unwrap() = msg.clone();
                broadcast(&shared_bg, ServerMsg::Status {
                    state,
                    message: msg,
                });
                result
            })?;

        // Error broadcast thread: drains err_rx and broadcasts to clients
        let shared_err = shared.clone();
        thread::spawn(move || {
            while let Ok(error) = err_rx.recv() {
                broadcast(&shared_err, ServerMsg::Error { text: error });
            }
        });

        let plugin_infos: Vec<crate::error_gate::PluginInfo> = self
            .plugins
            .iter()
            .map(|p| crate::error_gate::PluginInfo {
                name: p.name().to_string(),
                description: p.description().to_string(),
            })
            .collect();
        let command_infos = self.command_infos();
        let plugins = self.plugins;

        // Accept loop
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Check if we should exit
            if exit_flag.load(Ordering::Acquire) {
                break;
            }
            if bg_handle.is_finished() && shared.clients.lock().unwrap().is_empty() {
                break;
            }

            let plugins_ref = &plugins;
            let plugin_infos_ref = &plugin_infos;
            let command_infos_ref = &command_infos;
            let shared_cloned = shared.clone();
            let epoch_ref = &epoch;
            let exit_flag_ref = &exit_flag;

            // Handle client in this thread (blocking, one client at a time)
            handle_client(
                stream,
                plugins_ref,
                plugin_infos_ref,
                command_infos_ref,
                shared_cloned,
                epoch_ref,
                exit_flag_ref,
            );

            if exit_flag.load(Ordering::Acquire) {
                break;
            }
        }

        // Cleanup
        shutdown.store(true, Ordering::Release);
        epoch.fetch_add(1, Ordering::Release);
        let _ = std::fs::remove_file(&self.socket_path);

        // Wait for bg thread
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = done_tx.send(bg_handle.join());
        });
        match done_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => {
                eprintln!("[background work error: {e:#}]");
                Ok(())
            }
            _ => {
                std::process::exit(0);
            }
        }
    }
}

fn handle_client(
    stream: UnixStream,
    plugins: &[Box<dyn ShellPlugin>],
    plugin_infos: &[crate::error_gate::PluginInfo],
    command_infos: &[CommandInfo],
    shared: Arc<SharedState>,
    epoch: &Arc<AtomicU64>,
    exit_flag: &Arc<AtomicBool>,
) {
    // Register client for broadcasts
    {
        let mut clients = shared.clients.lock().unwrap();
        clients.push(stream.try_clone().unwrap());
    }

    // Send current status
    let _ = write_msg(
        &mut std::io::BufWriter::new(stream.try_clone().unwrap()),
        &ServerMsg::Commands {
            list: command_infos.to_vec(),
        },
    );

    let reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = std::io::BufWriter::new(stream);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }

        let msg: ClientMsg = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        match msg {
            ClientMsg::Command { name, args } => {
                if name == "help" {
                    let mut text = String::from("Available commands:");
                    for ci in command_infos {
                        text.push_str(&format!("\n  {:<14} {}", ci.name, ci.description));
                    }
                    let _ = write_msg(&mut writer, &ServerMsg::Output { text, kind: OutKind::Output });
                } else {
                    let ctx = ShellContext::new(epoch, exit_flag, plugin_infos);
                    match plugins.iter().find(|p| p.name() == name) {
                        Some(plugin) => {
                            let result = plugin.handle(&args, &ctx);
                            for line in result.lines() {
                                let kind = if line.starts_with("--- ERROR") {
                                    OutKind::Error
                                } else {
                                    OutKind::Output
                                };
                                let _ = write_msg(&mut writer, &ServerMsg::Output {
                                    text: line.to_string(),
                                    kind,
                                });
                            }
                        }
                        None => {
                            let _ = write_msg(&mut writer, &ServerMsg::Output {
                                text: format!("[unknown command: '{name}' — type 'help']"),
                                kind: OutKind::Info,
                            });
                        }
                    }
                }

                let _ = write_msg(&mut writer, &ServerMsg::Done);
                let _ = writer.flush();

                if exit_flag.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    }

    // Client removed by broadcast when writes fail
}

fn broadcast(shared: &SharedState, msg: ServerMsg) {
    let mut clients = shared.clients.lock().unwrap();
    clients.retain(|c| {
        let mut writer = std::io::BufWriter::new(c.try_clone().unwrap());
        write_msg(&mut writer, &msg).is_ok()
    });
}
