use std::future::Future;

use crate::adapters::util::command_exists;
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};
use crate::subprocess::run_command;

pub struct PipCacheAdapter;

impl CacheAdapter for PipCacheAdapter {
    fn name(&self) -> &str {
        "pip"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move {
            if !command_exists("pip3") {
                return Vec::new();
            }
            let path = match pip_cache_dir().await {
                Some(p) => p,
                None => return Vec::new(),
            };
            if !std::path::Path::new(&path).exists() {
                return Vec::new();
            }
            let size = dir_size(&path);
            vec![CacheInfo {
                name: "pip cache".into(),
                path,
                size,
                requires_sudo: false,
            }]
        }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("pip3") {
                return Vec::new();
            }
            let path = match pip_cache_dir().await {
                Some(p) => p,
                None => return Vec::new(),
            };
            if !std::path::Path::new(&path).exists() {
                return Vec::new();
            }
            let size = dir_size(&path);
            if size == 0 {
                return Vec::new();
            }
            let mut suggestions = Vec::new();
            if let Some(mut s) = CleanupSuggestion::new(
                "Purge pip download cache".into(),
                size,
                "pip3 cache purge".into(),
                false,
                RiskLevel::Safe,
            ) {
                s.targets.push(path);
                suggestions.push(s);
            }
            suggestions
        }
    }
}

async fn pip_cache_dir() -> Option<String> {
    run_command("pip3", &["cache", "dir"], 5)
        .await
        .ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|p| !p.is_empty())
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
