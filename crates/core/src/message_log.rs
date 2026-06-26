use std::fmt;
use std::sync::Arc;

use dashmap::DashMap;
use tracing::field::Visit;
use tracing_subscriber::layer::Layer;

use crate::error_gate::{ShellContext, ShellPlugin};

pub struct LogEntry {
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

pub struct MessageLog {
    entries: DashMap<String, Vec<LogEntry>>,
}

impl Default for MessageLog {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageLog {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    pub fn push(&self, context_id: String, entry: LogEntry) {
        self.entries
            .entry(context_id)
            .or_default()
            .push(entry);
    }

    pub fn get(&self, context_id: &str) -> Vec<LogEntry> {
        self.entries
            .get(context_id)
            .map(|r| r.value().clone())
            .unwrap_or_default()
    }

    pub fn keys(&self) -> Vec<String> {
        self.entries.iter().map(|r| r.key().clone()).collect()
    }
}

struct EventVisitor {
    context_ids: Vec<String>,
    message: String,
    fields: Vec<(String, String)>,
}

const CTX_FIELD_NAMES: &[&str] = &["ctx", "context_id", "func_ctx", "fctx"];

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        let formatted = format!("{:?}", value);
        if field.name() == "message" {
            self.message = formatted;
        } else if CTX_FIELD_NAMES.contains(&field.name()) {
            self.context_ids.push(formatted);
        } else {
            self.fields.push((field.name().to_string(), formatted));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else if CTX_FIELD_NAMES.contains(&field.name()) {
            self.context_ids.push(value.to_string());
        } else {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if CTX_FIELD_NAMES.contains(&field.name()) {
            self.context_ids.push(value.to_string());
        } else {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if CTX_FIELD_NAMES.contains(&field.name()) {
            self.context_ids.push(value.to_string());
        } else {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields.push((field.name().to_string(), value.to_string()));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields.push((field.name().to_string(), value.to_string()));
    }

    fn record_bytes(&mut self, field: &tracing::field::Field, value: &[u8]) {
        self.fields.push((field.name().to_string(), format!("{:?}", value)));
    }
}

pub struct MessageLogLayer {
    log: Arc<MessageLog>,
}

impl MessageLogLayer {
    pub fn new(log: Arc<MessageLog>) -> Self {
        Self { log }
    }
}

impl<S: tracing::Subscriber> Layer<S> for MessageLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = EventVisitor {
            context_ids: Vec::new(),
            message: String::new(),
            fields: Vec::new(),
        };
        event.record(&mut visitor);

        if visitor.context_ids.is_empty() {
            return;
        }

        let level = *event.metadata().level();
        let target = event.metadata().target().to_string();
        let entry = LogEntry {
            level,
            target,
            message: visitor.message,
            fields: visitor.fields,
        };

        for ctx_id in visitor.context_ids {
            self.log.push(ctx_id, entry.clone());
        }
    }
}

impl Clone for LogEntry {
    fn clone(&self) -> Self {
        Self {
            level: self.level,
            target: self.target.clone(),
            message: self.message.clone(),
            fields: self.fields.clone(),
        }
    }
}

fn ancestors(ctx_id: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = ctx_id;
    loop {
        result.push(current.to_string());
        match current.rfind('.') {
            Some(pos) => current = &current[..pos],
            None => break,
        }
    }
    result
}

fn is_valid_ctx_id(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
}

pub struct ShowPlugin {
    log: Arc<MessageLog>,
}

impl ShowPlugin {
    pub fn new(log: Arc<MessageLog>) -> Self {
        Self { log }
    }
}

impl ShellPlugin for ShowPlugin {
    fn name(&self) -> &str {
        "show"
    }

    fn description(&self) -> &str {
        "Show message history for a piece id and its ancestors (e.g. 'show 1.2.3'), or list all ids with no args"
    }

    fn handle(&self, args: &str, _ctx: &ShellContext<'_>) -> String {
        let ctx_id = args.trim();
        if ctx_id.is_empty() {
            let keys = self.log.keys();
            if keys.is_empty() {
                return "No pieces recorded yet".to_string();
            }
            let mut top_level: Vec<String> = keys
                .iter()
                .filter(|k| !k.contains('.'))
                .cloned()
                .collect();
            top_level.sort_by(|a, b| {
                let na: u64 = a.parse().unwrap_or(0);
                let nb: u64 = b.parse().unwrap_or(0);
                na.cmp(&nb)
            });
            let mut out = String::new();
            for top in &top_level {
                let children: Vec<&String> = keys
                    .iter()
                    .filter(|k| k.starts_with(top.as_str()) && k.as_str() != top.as_str())
                    .collect();
                out.push_str(top);
                out.push('\n');
                for child in &children {
                    out.push_str("  ");
                    out.push_str(child);
                    out.push('\n');
                }
            }
            out
        } else if !is_valid_ctx_id(ctx_id) {
            return format!("Invalid piece id: '{ctx_id}' — expected dotted number like 1.2.3");
        } else {
            self.show_piece(ctx_id)
        }
    }
}

impl ShowPlugin {
    fn show_piece(&self, ctx_id: &str) -> String {
        let ancestors = ancestors(ctx_id);
        let mut out = String::new();

        for ancestor in &ancestors {
            let entries = self.log.get(ancestor);
            if entries.is_empty() {
                continue;
            }

            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("--- {} ---\n", ancestor));
            for entry in &entries {
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
        }

        if out.is_empty() {
            format!("No messages found for {ctx_id} or its ancestors")
        } else {
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ancestors_leaf() {
        assert_eq!(ancestors("1.2.3"), vec!["1.2.3", "1.2", "1"]);
    }

    #[test]
    fn test_ancestors_root_child() {
        assert_eq!(ancestors("5"), vec!["5"]);
    }

    #[test]
    fn test_ancestors_deep() {
        assert_eq!(ancestors("1.2.3.4"), vec!["1.2.3.4", "1.2.3", "1.2", "1"]);
    }

    #[test]
    fn test_is_valid_ctx_id() {
        assert!(is_valid_ctx_id("1"));
        assert!(is_valid_ctx_id("1.2.3"));
        assert!(!is_valid_ctx_id(""));
        assert!(!is_valid_ctx_id("abc"));
        assert!(!is_valid_ctx_id("1.2."));
        assert!(!is_valid_ctx_id(".1"));
        assert!(!is_valid_ctx_id("1.a.2"));
    }

    #[test]
    fn test_message_log_push_and_get() {
        let log = MessageLog::new();
        let entry = LogEntry {
            level: tracing::Level::INFO,
            target: "test".to_string(),
            message: "hello".to_string(),
            fields: vec![],
        };
        log.push("1.2".to_string(), entry);
        let entries = log.get("1.2");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "hello");
        assert!(log.get("9.9").is_empty());
    }

    #[test]
    fn test_show_plugin_invalid_input() {
        let log = Arc::new(MessageLog::new());
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let ctx = ShellContext::new(&epoch, &infos);
        let result = plugin.handle("", &ctx);
        assert!(result.contains("No pieces recorded"));

        let result = plugin.handle("abc", &ctx);
        assert!(result.contains("Invalid"));
    }

    #[test]
    fn test_show_plugin_list_ids() {
        let log = Arc::new(MessageLog::new());
        log.push(
            "2".to_string(),
            LogEntry {
                level: tracing::Level::INFO,
                target: "test".to_string(),
                message: "msg".to_string(),
                fields: vec![],
            },
        );
        log.push(
            "2.1".to_string(),
            LogEntry {
                level: tracing::Level::INFO,
                target: "test".to_string(),
                message: "msg".to_string(),
                fields: vec![],
            },
        );
        log.push(
            "1".to_string(),
            LogEntry {
                level: tracing::Level::INFO,
                target: "test".to_string(),
                message: "msg".to_string(),
                fields: vec![],
            },
        );
        log.push(
            "1.2".to_string(),
            LogEntry {
                level: tracing::Level::INFO,
                target: "test".to_string(),
                message: "msg".to_string(),
                fields: vec![],
            },
        );
        log.push(
            "1.2.3".to_string(),
            LogEntry {
                level: tracing::Level::INFO,
                target: "test".to_string(),
                message: "msg".to_string(),
                fields: vec![],
            },
        );
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let ctx = ShellContext::new(&epoch, &infos);
        let result = plugin.handle("", &ctx);
        assert!(result.contains("1"));
        assert!(result.contains("2"));
        assert!(result.contains("1.2"));
        assert!(result.contains("1.2.3"));
        assert!(result.contains("2.1"));
    }

    #[test]
    fn test_show_plugin_no_messages() {
        let log = Arc::new(MessageLog::new());
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let ctx = ShellContext::new(&epoch, &infos);
        let result = plugin.handle("1.2.3", &ctx);
        assert!(result.contains("No messages found"));
    }

    #[test]
    fn test_show_plugin_with_entries() {
        let log = Arc::new(MessageLog::new());
        log.push(
            "1".to_string(),
            LogEntry {
                level: tracing::Level::INFO,
                target: "test".to_string(),
                message: "root msg".to_string(),
                fields: vec![],
            },
        );
        log.push(
            "1.2".to_string(),
            LogEntry {
                level: tracing::Level::WARN,
                target: "test".to_string(),
                message: "warn msg".to_string(),
                fields: vec![("attempt".to_string(), "1".to_string())],
            },
        );
        let plugin = ShowPlugin::new(log.clone());
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let ctx = ShellContext::new(&epoch, &infos);
        let result = plugin.handle("1.2", &ctx);
        assert!(result.contains("--- 1.2 ---"));
        assert!(result.contains("--- 1 ---"));
        assert!(result.contains("root msg"));
        assert!(result.contains("warn msg"));
        assert!(result.contains("attempt=1"));
    }
}
