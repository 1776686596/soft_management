use std::future::Future;
use std::path::Path;

use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};

pub struct LogCacheAdapter;

const LARGE_LOG_THRESHOLD_BYTES: u64 = 100 * 1024 * 1024;
const MAX_LOG_SUGGESTIONS: usize = 20;

impl CacheAdapter for LogCacheAdapter {
    fn name(&self) -> &str {
        "logs"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move { Vec::new() }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            let root = Path::new("/var/log");
            if !root.exists() {
                return Vec::new();
            }

            let journal_root = Path::new("/var/log/journal");
            let mut candidates: Vec<(u64, String)> = Vec::new();

            for entry in walkdir::WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| !e.path().starts_with(journal_root))
                .filter_map(Result::ok)
            {
                let path = entry.path();

                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let size = metadata.len();
                if size < LARGE_LOG_THRESHOLD_BYTES {
                    continue;
                }

                let path_str = path.to_string_lossy().to_string();
                if path_str.chars().any(char::is_whitespace) {
                    continue;
                }
                candidates.push((size, path_str));
            }

            candidates.sort_by(|a, b| b.0.cmp(&a.0));
            candidates.truncate(MAX_LOG_SUGGESTIONS);

            candidates
                .into_iter()
                .filter_map(|(size, path)| {
                    let mut suggestion = CleanupSuggestion::new(
                        format!("Truncate log file: {path}"),
                        size,
                        format!("truncate -s 0 {path}"),
                        true,
                        RiskLevel::Moderate,
                    )?;
                    suggestion.targets.push(path);
                    Some(suggestion)
                })
                .collect()
        }
    }
}
