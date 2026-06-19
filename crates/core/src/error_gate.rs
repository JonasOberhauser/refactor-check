use std::future::Future;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::config_update::{ApplyTo, SetPlugin};
use crate::live_config::LiveConfig;

pub struct ErrorGate {
    epoch: Arc<AtomicU64>,
    tx: mpsc::Sender<String>,
}

impl ErrorGate {
    fn new(epoch: Arc<AtomicU64>, tx: mpsc::Sender<String>) -> Self {
        Self { epoch, tx }
    }

    pub async fn report_and_wait(&self, error: &str) {
        let my_epoch = self.epoch.load(Ordering::Acquire);
        let _ = self.tx.send(error.to_string());
        loop {
            if self.epoch.load(Ordering::Acquire) != my_epoch {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

pub struct PluginInfo {
    pub name: String,
    pub description: String,
}

pub struct ShellContext<'a> {
    epoch: &'a Arc<AtomicU64>,
    plugin_infos: &'a [PluginInfo],
}

impl<'a> ShellContext<'a> {
    pub fn resume(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::Release)
    }

    pub fn exit(&self) -> ! {
        eprintln!("[exiting...]");
        std::process::exit(0);
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

struct ContinuePlugin;

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

struct ExitPlugin;

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

struct HelpPlugin;

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
        let (tx, rx) = mpsc::channel::<String>();
        let gate = Arc::new(ErrorGate::new(epoch.clone(), tx));

        let bg_handle = std::thread::Builder::new()
            .name("verification".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(work(Some(gate)))
            })?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("stdin".to_string())
            .spawn(move || {
                let stdin = std::io::stdin();
                let mut buf = String::new();
                loop {
                    buf.clear();
                    match stdin.read_line(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => {
                            let _ = cmd_tx.send(buf.trim().to_string());
                        }
                        Err(_) => break,
                    }
                }
            })?;

        let plugin_infos: Vec<PluginInfo> = self
            .plugins
            .iter()
            .map(|p| PluginInfo {
                name: p.name().to_string(),
                description: p.description().to_string(),
            })
            .collect();

        eprintln!("[Interactive error shell — type 'help' for commands]");

        let plugins = self.plugins;

        loop {
            while let Ok(error) = rx.try_recv() {
                eprintln!("\n--- ERROR ---\n{error}\n-------------");
                eprint!("> ");
                let _ = std::io::stderr().flush();
            }

            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        if cmd.is_empty() {
                            continue;
                        }

                        let (name, args) = match cmd.split_once(char::is_whitespace) {
                            Some((n, a)) => (n, a.trim()),
                            None => (cmd.as_str(), ""),
                        };

                        let ctx = ShellContext {
                            epoch: &epoch,
                            plugin_infos: &plugin_infos,
                        };

                        match plugins.iter().find(|p| p.name() == name) {
                            Some(plugin) => {
                                let msg = plugin.handle(args, &ctx);
                                if !msg.is_empty() {
                                    eprintln!("{msg}");
                                }
                                eprint!("> ");
                                let _ = std::io::stderr().flush();
                            }
                            None => {
                                eprintln!(
                                    "[unknown command: '{cmd}' — type 'help' for available commands]"
                                );
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        eprintln!("[EOF — exiting]");
                        std::process::exit(0);
                    }
                }
            }

            if bg_handle.is_finished() {
                break;
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        bg_handle
            .join()
            .map_err(|_| anyhow::anyhow!("background thread panicked"))?
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
        let infos: Vec<PluginInfo> = vec![];
        let ctx = ShellContext {
            epoch: &epoch,
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
