use std::future::Future;

use crate::adapters::util::command_exists;
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};
use crate::subprocess::run_command;

pub struct NpmCacheAdapter;

impl CacheAdapter for NpmCacheAdapter {
    fn name(&self) -> &str {
        "npm"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move {
            if !command_exists("npm") {
                return Vec::new();
            }
            let path = match npm_cache_dir().await {
                Some(p) => p,
                None => return Vec::new(),
            };
            if !std::path::Path::new(&path).exists() {
                return Vec::new();
            }
            let size = dir_size(&path);
            vec![CacheInfo {
                name: "npm cache".into(),
                path,
                size,
                requires_sudo: false,
            }]
        }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("npm") {
                return Vec::new();
            }
            let path = match npm_cache_dir().await {
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
                "Clean npm cache".into(),
                size,
                "npm cache clean --force".into(),
                false,
                RiskLevel::Moderate,
            ) {
                s.targets.push(path);
                suggestions.push(s);
            }
            suggestions
        }
    }
}

async fn npm_cache_dir() -> Option<String> {
    run_command("npm", &["config", "get", "cache"], 5)
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
