use std::future::Future;

use crate::adapters::util::{command_exists, parse_human_size_to_bytes};
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};
use crate::subprocess::run_command;

pub struct JournalCacheAdapter;

impl CacheAdapter for JournalCacheAdapter {
    fn name(&self) -> &str {
        "journal"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move { Vec::new() }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("journalctl") {
                return Vec::new();
            }

            let size = match journal_disk_usage_bytes().await {
                Some(s) if s > 0 => s,
                _ => return Vec::new(),
            };

            let mut suggestions = Vec::new();
            if let Some(mut s) = CleanupSuggestion::new(
                "Vacuum systemd journal (keep 7 days)".into(),
                size,
                "journalctl --vacuum-time=7d".into(),
                true,
                RiskLevel::Moderate,
            ) {
                s.targets.push("/var/log/journal".into());
                s.targets.push("/run/log/journal".into());
                suggestions.push(s);
            }
            if let Some(mut s) = CleanupSuggestion::new(
                "Vacuum systemd journal (limit to 200MB)".into(),
                size,
                "journalctl --vacuum-size=200M".into(),
                true,
                RiskLevel::Moderate,
            ) {
                s.targets.push("/var/log/journal".into());
                s.targets.push("/run/log/journal".into());
                suggestions.push(s);
            }
            suggestions
        }
    }
}

async fn journal_disk_usage_bytes() -> Option<u64> {
    let output = run_command("journalctl", &["--disk-usage"], 15).await.ok()?;
    output
        .stdout
        .split_whitespace()
        .filter_map(parse_human_size_to_bytes)
        .find(|bytes| *bytes > 0)
}
