use std::fmt;
use std::future::Future;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::Editor;

use crate::config_update::{ApplyTo, SetPlugin};
use crate::live_config::LiveConfig;

/// Returned by [`ErrorGate::report_and_wait`] when the shell is shutting down.
/// Callers should propagate this error to unwind the background thread.
#[derive(Debug)]
pub struct ShutdownRequested;

impl fmt::Display for ShutdownRequested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "shutdown requested")
    }
}

impl std::error::Error for ShutdownRequested {}

pub struct ErrorGate {
    epoch: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    tx: mpsc::Sender<String>,
}

impl ErrorGate {
    pub(crate) fn new(epoch: Arc<AtomicU64>, shutdown: Arc<AtomicBool>, tx: mpsc::Sender<String>) -> Self {
        Self { epoch, shutdown, tx }
    }

    pub async fn report_and_wait(&self, error: &str) -> Result<(), ShutdownRequested> {
        let my_epoch = self.epoch.load(Ordering::Acquire);
        let _ = self.tx.send(error.to_string());
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return Err(ShutdownRequested);
            }
            if self.epoch.load(Ordering::Acquire) != my_epoch {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(())
    }
}

pub struct PluginInfo {
    pub name: String,
    pub description: String,
}

pub struct ShellContext<'a> {
    epoch: &'a Arc<AtomicU64>,
    exit_flag: &'a AtomicBool,
    plugin_infos: &'a [PluginInfo],
}

impl<'a> ShellContext<'a> {
    pub fn new(epoch: &'a Arc<AtomicU64>, exit_flag: &'a AtomicBool, plugin_infos: &'a [PluginInfo]) -> Self {
        Self { epoch, exit_flag, plugin_infos }
    }

    pub fn resume(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::Release)
    }

    pub fn exit(&self) -> String {
        println!("[exiting...]");
        self.exit_flag.store(true, Ordering::Release);
        String::new()
    }

    pub fn plugins(&self) -> &'a [PluginInfo] {
        self.plugin_infos
    }
}

pub trait ShellPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    fn handle(&self, args: &str, ctx: &ShellContext<'_>) -> String;
}

pub struct ContinuePlugin;

impl ShellPlugin for ContinuePlugin {
    fn name(&self) -> &str {
        "continue"
    }
    fn description(&self) -> &str {
        "Resume blocked tasks after error"
    }
    fn handle(&self, _args: &str, ctx: &ShellContext<'_>) -> String {
        let old = ctx.resume();
        format!("[epoch {} -> {} — resuming blocked tasks]", old, old + 1)
    }
}

pub struct ExitPlugin;

impl ShellPlugin for ExitPlugin {
    fn name(&self) -> &str {
        "exit"
    }
    fn description(&self) -> &str {
        "Quit the process"
    }
    fn handle(&self, _args: &str, ctx: &ShellContext<'_>) -> String {
        ctx.exit()
    }
}

pub struct HelpPlugin;

impl ShellPlugin for HelpPlugin {
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> &str {
        "Show available commands"
    }
    fn handle(&self, _args: &str, ctx: &ShellContext<'_>) -> String {
        let mut out = String::from("Available commands:\n");
        for plugin in ctx.plugins() {
            let desc = if plugin.description.is_empty() {
                ""
            } else {
                plugin.description.as_str()
            };
            out.push_str(&format!("  {:<12} {}\n", plugin.name, desc));
        }
        out
    }
}

struct ShellHelper {
    commands: Vec<String>,
}

impl ShellHelper {
    fn new(commands: Vec<String>) -> Self {
        Self { commands }
    }
}

impl Completer for ShellHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let prefix = &line[..pos];
        let matches: Vec<String> = self
            .commands
            .iter()
            .filter(|cmd| cmd.starts_with(prefix))
            .cloned()
            .collect();
        Ok((0, matches))
    }
}

impl Hinter for ShellHelper {
    type Hint = String;
}

impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl rustyline::Helper for ShellHelper {}

pub struct ErrorShell {
    plugins: Vec<Box<dyn ShellPlugin>>,
}

impl ErrorShell {
    #[must_use]
    pub fn new() -> Self {
        Self { plugins: Vec::new() }
    }

    #[must_use]
    pub fn with_base_plugins() -> Self {
        let mut shell = Self::new();
        shell.register_plugin_boxed(Box::new(ContinuePlugin));
        shell.register_plugin_boxed(Box::new(ExitPlugin));
        shell.register_plugin_boxed(Box::new(HelpPlugin));
        shell
    }

    pub fn with_config<A, C>(mut self, name: &'static str, config: Arc<LiveConfig<C>>) -> Result<Self>
    where
        A: ApplyTo<C>,
        C: Clone + Send + Sync + 'static,
    {
        self.register_plugin(Box::new(SetPlugin::<A, C>::new(name, config)))?;
        Ok(self)
    }

    pub fn with_plugin(mut self, plugin: Box<dyn ShellPlugin>) -> Result<Self> {
        self.register_plugin(plugin)?;
        Ok(self)
    }

    fn register_plugin_boxed(&mut self, plugin: Box<dyn ShellPlugin>) {
        let name = plugin.name().to_string();
        if self.plugins.iter().any(|p| p.name() == name.as_str()) {
            panic!("plugin '{name}' already registered");
        }
        self.plugins.push(plugin);
    }

    pub fn register_plugin(&mut self, plugin: Box<dyn ShellPlugin>) -> Result<()> {
        let name = plugin.name().to_string();
        if self.plugins.iter().any(|p| p.name() == name.as_str()) {
            bail!("plugin '{name}' already registered");
        }
        self.plugins.push(plugin);
        Ok(())
    }
}

fn shutdown_bg(
    shutdown: &Arc<AtomicBool>,
    epoch: &Arc<AtomicU64>,
    bg_handle: std::thread::JoinHandle<Result<()>>,
) {
    shutdown.store(true, Ordering::Release);
    epoch.fetch_add(1, Ordering::Release);

    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(bg_handle.join());
    });
    match done_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => {
            println!("[background work error: {e:#}]");
        }
        Ok(Err(_)) => {
            println!("[background thread panicked]");
        }
        Err(_) => {
            // Force exit — don't let blocking I/O in the bg thread prevent exit
            std::process::exit(0);
        }
    }
}

impl ErrorShell {
    pub fn run<F, Fut>(self, work: F) -> Result<()>
    where
        F: FnOnce(Option<Arc<ErrorGate>>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        if !std::io::stdin().is_terminal() {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            return rt.block_on(work(None));
        }

        let epoch = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let exit_flag = AtomicBool::new(false);
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

        let plugin_infos: Vec<PluginInfo> = self
            .plugins
            .iter()
            .map(|p| PluginInfo {
                name: p.name().to_string(),
                description: p.description().to_string(),
            })
            .collect();

        let command_names: Vec<String> = plugin_infos.iter().map(|p| p.name.clone()).collect();
        let plugins = self.plugins;

        let mut editor = Editor::new()?;
        editor.set_helper(Some(ShellHelper::new(command_names)));

        println!("[Interactive error shell — type 'help' for commands]");

        // Run readline on a dedicated thread so the main loop can poll for
        // background errors/completion without blocking.
        enum ShellInput {
            Line(String),
            Eof,
            Interrupted,
            Error(rustyline::error::ReadlineError),
        }
        let (input_tx, input_rx) = mpsc::channel::<ShellInput>();
        let editor_prompt = "$ ".to_string();
        std::thread::spawn(move || {
            loop {
                match editor.readline(&editor_prompt) {
                    Ok(line) => {
                        if input_tx.send(ShellInput::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Interrupted) => {
                        if input_tx.send(ShellInput::Interrupted).is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Eof) => {
                        let _ = input_tx.send(ShellInput::Eof);
                        break;
                    }
                    Err(e) => {
                        let _ = input_tx.send(ShellInput::Error(e));
                        break;
                    }
                }
            }
        });

        let mut bg_done = false;
        let mut bg_handle_opt = Some(bg_handle);
        loop {
            // Drain and display errors from background thread
            while let Ok(error) = rx.try_recv() {
                println!("\n--- ERROR ---\n{error}\n-------------");
            }

            // Check if background thread finished
            if !bg_done {
                if let Some(ref handle) = bg_handle_opt {
                    if handle.is_finished() {
                        bg_done = true;
                        // Give error thread a moment to flush
                        std::thread::sleep(Duration::from_millis(50));
                        let handle = bg_handle_opt.take().unwrap();
                        match handle.join() {
                            Ok(Ok(())) => {
                                println!("[Background work finished — type 'exit' to quit]");
                            }
                            Ok(Err(e)) => {
                                println!("[Background work error: {e:#}]");
                            }
                            Err(_) => {
                                println!("[Background thread panicked — type 'exit' to quit]");
                            }
                        }
                    }
                }
            }

            // Wait for user input, but poll for background completion
            let input = match input_rx.recv_timeout(if bg_done { Duration::from_secs(60) } else { Duration::from_millis(500) }) {
                Ok(ShellInput::Line(line)) => line,
                Ok(ShellInput::Interrupted) => continue,
                Ok(ShellInput::Eof) => {
                    println!("[EOF — exiting]");
                    if let Some(handle) = bg_handle_opt.take() {
                        shutdown_bg(&shutdown, &epoch, handle);
                    }
                    return Ok(());
                }
                Ok(ShellInput::Error(e)) => return Err(e.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    println!("[input thread disconnected — exiting]");
                    if let Some(handle) = bg_handle_opt.take() {
                        shutdown_bg(&shutdown, &epoch, handle);
                    }
                    return Ok(());
                }
            };

            let cmd = input.trim();
            if cmd.is_empty() {
                continue;
            }

            let (name, args) = match cmd.split_once(char::is_whitespace) {
                Some((n, a)) => (n, a.trim()),
                None => (cmd, ""),
            };

            let ctx = ShellContext {
                epoch: &epoch,
                exit_flag: &exit_flag,
                plugin_infos: &plugin_infos,
            };

            match plugins.iter().find(|p| p.name() == name) {
                Some(plugin) => {
                    let msg = plugin.handle(args, &ctx);
                    if !msg.is_empty() {
                        println!("{msg}");
                    }
                }
                None => {
                    println!(
                        "[unknown command: '{cmd}' — type 'help' for available commands]"
                    );
                }
            }

            if exit_flag.load(Ordering::Acquire) {
                println!("[exiting]");
                if let Some(handle) = bg_handle_opt.take() {
                    shutdown_bg(&shutdown, &epoch, handle);
                }
                return Ok(());
            }
        }
    }
}

impl Default for ErrorShell {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_plugins_registered() {
        let shell = ErrorShell::with_base_plugins();
        assert_eq!(shell.plugins.len(), 3);
        let names: Vec<&str> = shell.plugins.iter().map(|p| p.name()).collect();
        assert!(names.contains(&"continue"));
        assert!(names.contains(&"exit"));
        assert!(names.contains(&"help"));
    }

    #[test]
    fn test_register_plugin_ok() {
        struct Echo;
        impl ShellPlugin for Echo {
            fn name(&self) -> &str {
                "echo"
            }
            fn handle(&self, args: &str, _ctx: &ShellContext<'_>) -> String {
                args.to_string()
            }
        }

        let mut shell = ErrorShell::with_base_plugins();
        assert!(shell.register_plugin(Box::new(Echo)).is_ok());
        assert_eq!(shell.plugins.len(), 4);
    }

    #[test]
    fn test_register_duplicate_returns_err() {
        struct Foo;
        impl ShellPlugin for Foo {
            fn name(&self) -> &str {
                "foo"
            }
            fn handle(&self, _args: &str, _ctx: &ShellContext<'_>) -> String {
                String::new()
            }
        }

        let mut shell = ErrorShell::new();
        assert!(shell.register_plugin(Box::new(Foo)).is_ok());
        let result = shell.register_plugin(Box::new(Foo));
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("already registered"));
    }

    #[test]
    fn test_continue_plugin_increments_epoch() {
        let epoch = Arc::new(AtomicU64::new(5));
        let exit_flag = AtomicBool::new(false);
        let infos: Vec<PluginInfo> = vec![];
        let ctx = ShellContext {
            epoch: &epoch,
            exit_flag: &exit_flag,
            plugin_infos: &infos,
        };
        let plugin = ContinuePlugin;
        let msg = plugin.handle("", &ctx);
        assert_eq!(epoch.load(Ordering::Acquire), 6);
        assert!(msg.contains("5"));
        assert!(msg.contains("6"));
    }

    #[test]
    fn test_help_plugin_lists_all() {
        let epoch = Arc::new(AtomicU64::new(0));
        let exit_flag = AtomicBool::new(false);
        let infos = vec![
            PluginInfo {
                name: "continue".to_string(),
                description: "Resume".to_string(),
            },
            PluginInfo {
                name: "exit".to_string(),
                description: "Quit".to_string(),
            },
        ];
        let ctx = ShellContext {
            epoch: &epoch,
            exit_flag: &exit_flag,
            plugin_infos: &infos,
        };
        let plugin = HelpPlugin;
        let msg = plugin.handle("", &ctx);
        assert!(msg.contains("continue"));
        assert!(msg.contains("exit"));
        assert!(msg.contains("Resume"));
        assert!(msg.contains("Quit"));
    }

    #[test]
    fn test_new_shell_has_no_plugins() {
        let shell = ErrorShell::new();
        assert!(shell.plugins.is_empty());
    }

    #[test]
    fn test_default_impl_matches_new() {
        let shell = ErrorShell::default();
        assert!(shell.plugins.is_empty());
    }
}
