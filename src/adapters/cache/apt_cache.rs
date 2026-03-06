use std::future::Future;

use crate::adapters::util::command_exists;
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};
use crate::subprocess::run_command;

pub struct AptCacheAdapter;

impl CacheAdapter for AptCacheAdapter {
    fn name(&self) -> &str {
        "apt"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move {
            let path = "/var/cache/apt/archives";
            if !std::path::Path::new(path).exists() {
                return Vec::new();
            }
            let size = dir_size(path);
            vec![CacheInfo {
                name: "APT package cache".into(),
                path: path.into(),
                size,
                requires_sudo: true,
            }]
        }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("apt") {
                return Vec::new();
            }
            let mut suggestions = Vec::new();

            let path = "/var/cache/apt/archives";
            if std::path::Path::new(path).exists() {
                let size = dir_size(path);
                if size > 0 {
                    if let Some(mut s) = CleanupSuggestion::new(
                        "Clean APT package cache".into(),
                        size,
                        "apt clean".into(),
                        true,
                        RiskLevel::Safe,
                    ) {
                        s.targets.push(path.to_string());
                        suggestions.push(s);
                    }
                }
            }

            if let Some(bytes) = estimate_apt_autoremove_bytes().await {
                if bytes > 0 {
                    if let Some(mut s) = CleanupSuggestion::new(
                        "Remove unused APT packages (autoremove --purge)".into(),
                        bytes,
                        "apt autoremove --purge".into(),
                        true,
                        RiskLevel::Moderate,
                    ) {
                        s.targets
                            .push("Unused packages and related configs".to_string());
                        suggestions.push(s);
                    }
                }
            }

            suggestions
        }
    }
}

async fn estimate_apt_autoremove_bytes() -> Option<u64> {
    if !command_exists("apt-get") {
        return None;
    }
    let output = run_command("apt-get", &["-s", "autoremove", "--purge"], 30)
        .await
        .ok()?;
    parse_apt_simulate_freed_bytes(&output.stdout)
}

fn parse_apt_simulate_freed_bytes(stdout: &str) -> Option<u64> {
    for line in stdout.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if !l.contains("After this operation") || !l.contains("freed") {
            continue;
        }

        let tokens: Vec<&str> = l.split_whitespace().collect();

        for win in tokens.windows(2) {
            let a = win[0].trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != ',');
            let b = win[1].trim_matches(|c: char| !c.is_ascii_alphanumeric());
            if a.is_empty() || b.is_empty() {
                continue;
            }
            if let Some(bytes) = crate::adapters::util::parse_human_size_to_bytes(&format!("{a} {b}"))
            {
                if bytes > 0 {
                    return Some(bytes);
                }
            }
        }
    }
    None
}

fn dir_size(path: &str) -> u64 {
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}
