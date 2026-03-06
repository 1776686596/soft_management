use std::future::Future;

use crate::adapters::util::{command_exists, file_size_if_regular};
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};
use crate::subprocess::run_command;

pub struct SnapCacheAdapter;

impl CacheAdapter for SnapCacheAdapter {
    fn name(&self) -> &str {
        "snap"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move { Vec::new() }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("snap") {
                return Vec::new();
            }

            let output = match run_command("snap", &["list", "--all"], 15).await {
                Ok(o) => o,
                Err(_) => return Vec::new(),
            };

            let mut suggestions = Vec::new();
            for line in output.stdout.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("Name ") {
                    continue;
                }

                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() < 6 {
                    continue;
                }

                let name = parts[0];
                let rev = parts[2];
                let notes = parts.last().copied().unwrap_or_default();

                if !notes.contains("disabled") {
                    continue;
                }
                if !rev.chars().all(|ch| ch.is_ascii_digit()) {
                    continue;
                }

                let snap_path = format!("/var/lib/snapd/snaps/{name}_{rev}.snap");
                let size = file_size_if_regular(&snap_path).unwrap_or(0);
                let cmd = format!("snap remove {name} --revision {rev}");

                if let Some(mut s) = CleanupSuggestion::new(
                    format!("Remove disabled snap revision: {name} (rev {rev})"),
                    size,
                    cmd,
                    true,
                    RiskLevel::Moderate,
                ) {
                    s.targets.push(snap_path);
                    suggestions.push(s);
                }
            }

            suggestions
        }
    }
}
