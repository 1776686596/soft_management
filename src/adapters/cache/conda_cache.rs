use std::future::Future;

use crate::adapters::util::command_exists;
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};

pub struct CondaCacheAdapter;

impl CacheAdapter for CondaCacheAdapter {
    fn name(&self) -> &str {
        "conda"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move {
            if !command_exists("conda") {
                return Vec::new();
            }
            let home = std::env::var("HOME").unwrap_or_default();
            let paths = [
                format!("{home}/anaconda3/pkgs"),
                format!("{home}/miniconda3/pkgs"),
            ];
            paths
                .into_iter()
                .filter_map(|path| {
                    if !std::path::Path::new(&path).exists() {
                        return None;
                    }
                    let size = dir_size(&path);
                    Some(CacheInfo {
                        name: "conda package cache".into(),
                        path,
                        size,
                        requires_sudo: false,
                    })
                })
                .collect()
        }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("conda") {
                return Vec::new();
            }
            let home = std::env::var("HOME").unwrap_or_default();
            let mut total: u64 = 0;
            let mut targets: Vec<String> = Vec::new();
            for path in [
                format!("{home}/anaconda3/pkgs"),
                format!("{home}/miniconda3/pkgs"),
            ]
            .into_iter()
            {
                if std::path::Path::new(&path).exists() {
                    total = total.saturating_add(dir_size(&path));
                    targets.push(path);
                }
            }
            if total == 0 {
                return Vec::new();
            }
            let mut suggestions = Vec::new();
            if let Some(mut s) = CleanupSuggestion::new(
                "Clean conda package cache and tarballs".into(),
                total,
                "conda clean --all -y".into(),
                false,
                RiskLevel::Safe,
            ) {
                s.targets = targets;
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
