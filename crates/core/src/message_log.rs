use std::sync::Arc;

pub use servyi_states::message_log::{MessageLog, ancestors, is_valid_ctx_id, LogEntry, MessageLogLayer};

use crate::error_gate::{ShellContext, ShellPlugin};

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
                let status = self.log.get_status(key).unwrap_or("-".to_string());
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
        } else if !is_valid_ctx_id(ctx_id) {
            format!("Invalid piece id: '{ctx_id}' — expected dotted number like 1.2.3")
        } else {
            self.show_piece(ctx_id)
        }
    }
}

impl ShowPlugin {
    fn show_piece(&self, ctx_id: &str) -> String {
        let ancestor_ids = ancestors(ctx_id);

        if !self.log.has(ctx_id) {
            return format!("No messages found for {ctx_id}");
        }

        let mut all_entries: Vec<(String, LogEntry)> = Vec::new();
        for ancestor in &ancestor_ids {
            for entry in self.log.get(ancestor) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn test_entry(log: &MessageLog, message: &str) -> LogEntry {
        LogEntry {
            seq: log.next_seq(),
            level: tracing::Level::INFO,
            target: "test".to_string(),
            message: message.to_string(),
            fields: vec![],
        }
    }

    #[test]
    fn test_show_plugin_invalid_input() {
        let log = Arc::new(MessageLog::new());
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let exit = AtomicBool::new(false);
        let ctx = ShellContext::new(&epoch, &exit, &infos);
        let result = plugin.handle("", &ctx);
        assert!(result.contains("No pieces recorded"));

        let result = plugin.handle("abc", &ctx);
        assert!(result.contains("Invalid"));
    }

    #[test]
    fn test_show_plugin_list_ids() {
        let log = Arc::new(MessageLog::new());
        log.push("2".to_string(), test_entry(&log, "msg"));
        log.push("2.1".to_string(), test_entry(&log, "msg"));
        log.push("1".to_string(), test_entry(&log, "msg"));
        log.push("1.2".to_string(), test_entry(&log, "msg"));
        log.push("1.2.3".to_string(), test_entry(&log, "msg"));
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let exit = AtomicBool::new(false);
        let ctx = ShellContext::new(&epoch, &exit, &infos);
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
        let exit = AtomicBool::new(false);
        let ctx = ShellContext::new(&epoch, &exit, &infos);
        let result = plugin.handle("1.2.3", &ctx);
        assert!(result.contains("No messages found"));
    }

    #[test]
    fn test_show_plugin_no_parents_for_nonexistent() {
        let log = Arc::new(MessageLog::new());
        log.push("1".to_string(), test_entry(&log, "root msg"));
        log.push("1.1".to_string(), test_entry(&log, "child1"));
        log.push("1.2".to_string(), test_entry(&log, "child2"));
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let exit = AtomicBool::new(false);
        let ctx = ShellContext::new(&epoch, &exit, &infos);
        let result = plugin.handle("1.3", &ctx);
        assert!(result.contains("No messages found"));
    }

    #[test]
    fn test_show_plugin_chronological_order() {
        let log = Arc::new(MessageLog::new());
        log.push("1".to_string(), test_entry(&log, "first"));
        log.push("1.2".to_string(), test_entry(&log, "second"));
        log.push("1".to_string(), test_entry(&log, "third"));
        log.push("1.2".to_string(), test_entry(&log, "fourth"));
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let exit = AtomicBool::new(false);
        let ctx = ShellContext::new(&epoch, &exit, &infos);
        let result = plugin.handle("1.2", &ctx);
        let first_pos = result.find("first").unwrap();
        let second_pos = result.find("second").unwrap();
        let third_pos = result.find("third").unwrap();
        let fourth_pos = result.find("fourth").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
        assert!(third_pos < fourth_pos);
    }

    #[test]
    fn test_show_plugin_with_entries() {
        let log = Arc::new(MessageLog::new());
        log.push("1".to_string(), test_entry(&log, "root msg"));
        log.push(
            "1.2".to_string(),
            LogEntry {
                seq: log.next_seq(),
                level: tracing::Level::WARN,
                target: "test".to_string(),
                message: "warn msg".to_string(),
                fields: vec![("attempt".to_string(), "1".to_string())],
            },
        );
        let plugin = ShowPlugin::new(log);
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let infos: Vec<crate::error_gate::PluginInfo> = vec![];
        let exit = AtomicBool::new(false);
        let ctx = ShellContext::new(&epoch, &exit, &infos);
        let result = plugin.handle("1.2", &ctx);
        assert!(result.contains("--- 1.2 ---"));
        assert!(result.contains("--- 1 ---"));
        assert!(result.contains("root msg"));
        assert!(result.contains("warn msg"));
        assert!(result.contains("attempt=1"));
    }
}
