use std::future::Future;

use crate::adapters::util::command_exists;
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, CleanupSuggestion, RiskLevel};
use crate::subprocess::run_command;

pub struct DockerCacheAdapter;

impl CacheAdapter for DockerCacheAdapter {
    fn name(&self) -> &str {
        "docker"
    }

    fn list_caches(&self) -> impl Future<Output = Vec<CacheInfo>> + Send {
        async move {
            if !command_exists("docker") {
                return Vec::new();
            }
            let size = match docker_disk_usage().await {
                Some(s) => s,
                None => return Vec::new(),
            };
            vec![CacheInfo {
                name: "Docker images and build cache".into(),
                path: "/var/lib/docker".into(),
                size,
                requires_sudo: true,
            }]
        }
    }

    fn suggest_cleanups(&self) -> impl Future<Output = Vec<CleanupSuggestion>> + Send {
        async move {
            if !command_exists("docker") {
                return Vec::new();
            }
            let size = match docker_disk_usage().await {
                Some(s) if s > 0 => s,
                _ => return Vec::new(),
            };
            let mut suggestions = Vec::new();
            if let Some(mut s) = CleanupSuggestion::new(
                "Prune unused Docker data".into(),
                size,
                "docker system prune -f".into(),
                false,
                RiskLevel::Moderate,
            ) {
                s.targets
                    .push("Unused containers, networks, images, and build cache".into());
                suggestions.push(s);
            }
            if let Some(mut s) = CleanupSuggestion::new(
                "Prune ALL Docker data (including volumes)".into(),
                size,
                "docker system prune -a --volumes".into(),
                true,
                RiskLevel::Moderate,
            ) {
                s.targets.push(
                    "Unused containers, networks, images, build cache, and unused volumes".into(),
                );
                suggestions.push(s);
            }
            suggestions
        }
    }
}

async fn docker_disk_usage() -> Option<u64> {
    let output = run_command("docker", &["system", "df", "--format", "{{json .}}"], 15)
        .await
        .ok()?;

    let mut total: u64 = 0;
    for line in output.stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(size_str) = entry.get("Size").and_then(|v| v.as_str()) {
                total += parse_docker_size(size_str);
            }
        }
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

fn parse_docker_size(s: &str) -> u64 {
    let s = s.trim();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix("GB") {
        (n.trim(), 1_073_741_824u64)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n.trim(), 1_048_576)
    } else if let Some(n) = s.strip_suffix("kB") {
        (n.trim(), 1024)
    } else if let Some(n) = s.strip_suffix("B") {
        (n.trim(), 1)
    } else {
        (s, 1)
    };
    num_str
        .parse::<f64>()
        .map(|n| (n * multiplier as f64) as u64)
        .unwrap_or(0)
}
