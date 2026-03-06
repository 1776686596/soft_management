use std::future::Future;

use crate::adapters::util::command_exists;
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};

pub struct CargoCacheAdapter;

impl CacheAdapter for CargoCacheAdapter {
    fn name(&self) -> &str {
        "cargo"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move {
            let home = std::env::var("HOME").unwrap_or_default();
            let registry = format!("{home}/.cargo/registry");
            if !std::path::Path::new(&registry).exists() {
                return Vec::new();
            }
            let size = dir_size(&registry);
            vec![CacheInfo {
                name: "cargo registry cache".into(),
                path: registry,
                size,
                requires_sudo: false,
            }]
        }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("cargo") {
                return Vec::new();
            }
            let home = std::env::var("HOME").unwrap_or_default();
            let registry = format!("{home}/.cargo/registry");
            if !std::path::Path::new(&registry).exists() {
                return Vec::new();
            }
            let size = dir_size(&registry);
            if size == 0 {
                return Vec::new();
            }
            let mut suggestions = Vec::new();
            if let Some(mut s) = CleanupSuggestion::new(
                "Auto-clean cargo registry cache".into(),
                size,
                "cargo cache --autoclean".into(),
                false,
                RiskLevel::Safe,
            ) {
                s.targets.push(registry);
                suggestions.push(s);
            }
            suggestions
        }
    }
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
