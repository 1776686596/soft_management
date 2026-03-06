use crate::adapters::cache::apt_cache::AptCacheAdapter;
use crate::adapters::cache::cargo_cache::CargoCacheAdapter;
use crate::adapters::cache::conda_cache::CondaCacheAdapter;
use crate::adapters::cache::docker_cache::DockerCacheAdapter;
use crate::adapters::cache::npm_cache::NpmCacheAdapter;
use crate::adapters::cache::pip_cache::PipCacheAdapter;
use crate::adapters::CacheAdapter;
use crate::models::{CacheInfo, Package};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct FolderUsage {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

pub struct DiskSnapshot {
    pub scan_id: u64,
    pub mode: ScanMode,
    pub is_final: bool,
    pub caches: Vec<CacheInfo>,
    pub roots: Vec<String>,
    pub folder_usage: HashMap<String, Vec<FolderUsage>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiskStage {
    ScanningCaches,
    AnalyzingRoots,
    Finished,
}

#[derive(Clone, Debug)]
pub struct DiskProgress {
    pub scan_id: u64,
    pub mode: ScanMode,
    pub stage: DiskStage,
    pub current: Option<String>,
    pub done: u32,
    pub total: u32,
    pub scanned_files: u64,
    pub elapsed_ms: u64,
    pub eta_ms: Option<u64>,
}

pub enum DiskEvent {
    Progress(DiskProgress),
    Snapshot(DiskSnapshot),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanMode {
    Fast,
    Full,
}

pub async fn scan_all(
    tx: async_channel::Sender<DiskEvent>,
    token: CancellationToken,
    mode: ScanMode,
    scan_id: u64,
) {
    let started = Instant::now();
    let send_progress = |progress: DiskProgress| {
        let _ = tx.try_send(DiskEvent::Progress(progress));
    };

    let adapters: Vec<Box<dyn CacheAdapterBoxed>> = vec![
        Box::new(AptCacheAdapter),
        Box::new(PipCacheAdapter),
        Box::new(NpmCacheAdapter),
        Box::new(CondaCacheAdapter),
        Box::new(CargoCacheAdapter),
        Box::new(DockerCacheAdapter),
    ];

    let adapter_total = u32::try_from(adapters.len()).unwrap_or(u32::MAX);
    let mut all_caches = Vec::new();

    for (idx, adapter) in adapters.iter().enumerate() {
        if token.is_cancelled() {
            return;
        }

        send_progress(DiskProgress {
            scan_id,
            mode,
            stage: DiskStage::ScanningCaches,
            current: Some(adapter.name().to_string()),
            done: u32::try_from(idx).unwrap_or(u32::MAX),
            total: adapter_total,
            scanned_files: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            eta_ms: None,
        });

        tracing::info!("scanning cache: {}", adapter.name());
        let caches = adapter.list_caches_boxed().await;
        if token.is_cancelled() {
            return;
        }

        all_caches.extend(caches);
    }

    let mut roots: Vec<String> = all_caches
        .iter()
        .map(|cache| normalize_path(&cache.path))
        .collect();
    roots.extend(system_scan_roots(mode));
    roots.sort();
    roots.dedup();

    let fast_roots: Vec<String> = roots
        .iter()
        .filter(|root| root.as_str() != "/")
        .cloned()
        .collect();

    let include_root_fs = mode == ScanMode::Full && roots.iter().any(|root| root == "/");
    let total_roots = u32::try_from(fast_roots.len() + if include_root_fs { 1 } else { 0 })
        .unwrap_or(u32::MAX);

    let mut folder_usage: HashMap<String, Vec<FolderUsage>> = HashMap::new();
    let mut roots_done = 0_u32;
    let mut completed_root_ms_total = 0_u64;
    let mut completed_root_count = 0_u32;

    for root in &fast_roots {
        if token.is_cancelled() {
            return;
        }

        tracing::info!("analyzing filesystem root: {}", root);
        let avg_root_ms = average_ms_per_root(completed_root_ms_total, completed_root_count);
        let remaining_roots = total_roots.saturating_sub(roots_done);
        send_progress(DiskProgress {
            scan_id,
            mode,
            stage: DiskStage::AnalyzingRoots,
            current: Some(root.clone()),
            done: roots_done,
            total: total_roots,
            scanned_files: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            eta_ms: avg_root_ms
                .map(|avg| avg.saturating_mul(u64::from(remaining_roots))),
        });

        let root_started = Instant::now();
        let root_for_worker = root.clone();
        let token_for_worker = token.clone();
        let tx_for_progress = tx.clone();
        let scan_id_for_progress = scan_id;
        let mode_for_progress = mode;
        let started_for_progress = started;
        let roots_done_for_progress = roots_done;
        let total_roots_for_progress = total_roots;
        let analyzed = tokio::task::spawn_blocking(move || {
            analyze_tree_entries(&root_for_worker, &token_for_worker, |p| {
                let eta_ms = avg_root_ms.map(|avg| {
                    avg.saturating_mul(u64::from(
                        total_roots_for_progress.saturating_sub(roots_done_for_progress),
                    ))
                });
                let _ = tx_for_progress.try_send(DiskEvent::Progress(DiskProgress {
                    scan_id: scan_id_for_progress,
                    mode: mode_for_progress,
                    stage: DiskStage::AnalyzingRoots,
                    current: Some(root_for_worker.clone()),
                    done: roots_done_for_progress,
                    total: total_roots_for_progress,
                    scanned_files: p.scanned_files,
                    elapsed_ms: started_for_progress.elapsed().as_millis() as u64,
                    eta_ms,
                }));
            })
        })
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("disk analyzer worker failed: {e}");
                HashMap::new()
            });

        if token.is_cancelled() {
            return;
        }

        for (parent, mut children) in analyzed {
            folder_usage
                .entry(parent)
                .or_default()
                .append(&mut children);
        }

        completed_root_ms_total =
            completed_root_ms_total.saturating_add(root_started.elapsed().as_millis() as u64);
        completed_root_count = completed_root_count.saturating_add(1);
        roots_done = roots_done.saturating_add(1);

        send_progress(DiskProgress {
            scan_id,
            mode,
            stage: DiskStage::AnalyzingRoots,
            current: Some(root.clone()),
            done: roots_done,
            total: total_roots,
            scanned_files: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            eta_ms: estimate_eta_ms(completed_root_ms_total, completed_root_count, total_roots, roots_done),
        });
    }

    sort_and_dedup_children(&mut folder_usage);

    let event = DiskSnapshot {
        scan_id,
        mode,
        is_final: !include_root_fs,
        caches: all_caches.clone(),
        roots: roots.clone(),
        folder_usage: folder_usage.clone(),
    };
    if tx.send(DiskEvent::Snapshot(event)).await.is_err() {
        return;
    }

    if include_root_fs {
        if token.is_cancelled() {
            return;
        }

        tracing::info!("analyzing filesystem root: /");
        let avg_root_ms = average_ms_per_root(completed_root_ms_total, completed_root_count);
        let remaining_roots = total_roots.saturating_sub(roots_done);
        send_progress(DiskProgress {
            scan_id,
            mode,
            stage: DiskStage::AnalyzingRoots,
            current: Some("/".to_string()),
            done: roots_done,
            total: total_roots,
            scanned_files: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            eta_ms: avg_root_ms
                .map(|avg| avg.saturating_mul(u64::from(remaining_roots))),
        });

        let root_started = Instant::now();
        let root_for_worker = "/".to_string();
        let token_for_worker = token.clone();
        let tx_for_progress = tx.clone();
        let scan_id_for_progress = scan_id;
        let mode_for_progress = mode;
        let started_for_progress = started;
        let roots_done_for_progress = roots_done;
        let total_roots_for_progress = total_roots;
        let analyzed = tokio::task::spawn_blocking(move || {
            analyze_tree_entries(&root_for_worker, &token_for_worker, |p| {
                let eta_ms = avg_root_ms.map(|avg| {
                    avg.saturating_mul(u64::from(
                        total_roots_for_progress.saturating_sub(roots_done_for_progress),
                    ))
                });
                let _ = tx_for_progress.try_send(DiskEvent::Progress(DiskProgress {
                    scan_id: scan_id_for_progress,
                    mode: mode_for_progress,
                    stage: DiskStage::AnalyzingRoots,
                    current: Some(root_for_worker.clone()),
                    done: roots_done_for_progress,
                    total: total_roots_for_progress,
                    scanned_files: p.scanned_files,
                    elapsed_ms: started_for_progress.elapsed().as_millis() as u64,
                    eta_ms,
                }));
            })
        })
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("disk analyzer worker failed: {e}");
                HashMap::new()
            });

        if token.is_cancelled() {
            return;
        }

        for (parent, mut children) in analyzed {
            folder_usage
                .entry(parent)
                .or_default()
                .append(&mut children);
        }

        sort_and_dedup_children(&mut folder_usage);

        completed_root_ms_total =
            completed_root_ms_total.saturating_add(root_started.elapsed().as_millis() as u64);
        completed_root_count = completed_root_count.saturating_add(1);
        roots_done = roots_done.saturating_add(1);

        send_progress(DiskProgress {
            scan_id,
            mode,
            stage: DiskStage::AnalyzingRoots,
            current: Some("/".to_string()),
            done: roots_done,
            total: total_roots,
            scanned_files: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
            eta_ms: estimate_eta_ms(completed_root_ms_total, completed_root_count, total_roots, roots_done),
        });

        let final_event = DiskSnapshot {
            scan_id,
            mode,
            is_final: true,
            caches: all_caches,
            roots,
            folder_usage,
        };
        let _ = tx.send(DiskEvent::Snapshot(final_event)).await;
    }

    send_progress(DiskProgress {
        scan_id,
        mode,
        stage: DiskStage::Finished,
        current: None,
        done: total_roots,
        total: total_roots,
        scanned_files: 0,
        elapsed_ms: started.elapsed().as_millis() as u64,
        eta_ms: Some(0),
    });
}

pub fn rank_packages(packages: &[Package], top_n: u32) -> Vec<&Package> {
    let top_n = top_n.clamp(10, 200) as usize;
    let mut sorted: Vec<&Package> = packages.iter().collect();
    sorted.sort_by(|a, b| match (a.size, b.size) {
        (Some(a_s), Some(b_s)) => b_s.cmp(&a_s),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    sorted.truncate(top_n);
    sorted
}

trait CacheAdapterBoxed: Send + Sync {
    fn name(&self) -> &str;
    fn list_caches_boxed(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<CacheInfo>> + Send + '_>>;
}

impl<T: CacheAdapter> CacheAdapterBoxed for T {
    fn name(&self) -> &str {
        CacheAdapter::name(self)
    }
    fn list_caches_boxed(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<CacheInfo>> + Send + '_>> {
        Box::pin(self.list_caches())
    }
}

pub struct AnalyzeProgress {
    pub scanned_files: u64,
}

pub fn analyze_tree_entries(
    root: &str,
    token: &CancellationToken,
    mut on_progress: impl FnMut(AnalyzeProgress),
) -> HashMap<String, Vec<FolderUsage>> {
    if token.is_cancelled() {
        return HashMap::new();
    }

    let root = normalize_path(root);
    let root_path = Path::new(&root);
    if !root_path.exists() || !root_path.is_dir() {
        return HashMap::new();
    }

    let mut dir_sizes: HashMap<String, u64> = HashMap::new();
    let mut files_by_parent: HashMap<String, Vec<FolderUsage>> = HashMap::new();

    dir_sizes.insert(root.clone(), 0);

    let mut scanned_files = 0_u64;
    let mut last_emit = Instant::now();

    for entry in walkdir::WalkDir::new(root_path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if token.is_cancelled() {
            break;
        }

        let path = entry.path();
        if should_skip_path(&root, path) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        let size = file_disk_usage_bytes(&metadata);
        scanned_files = scanned_files.saturating_add(1);
        let file_path = normalize_path(path.to_string_lossy().as_ref());
        let parent = path
            .parent()
            .and_then(|v| v.to_str())
            .map(normalize_path)
            .unwrap_or_else(|| root.clone());
        files_by_parent
            .entry(parent)
            .or_default()
            .push(FolderUsage {
                name: display_name(&file_path),
                path: file_path,
                size,
                is_dir: false,
            });

        let mut parent = path.parent();
        while let Some(dir) = parent {
            if !dir.starts_with(root_path) {
                break;
            }
            let dir_key = normalize_path(dir.to_string_lossy().as_ref());
            let current = dir_sizes.entry(dir_key).or_insert(0);
            *current = current.saturating_add(size);
            if dir == root_path {
                break;
            }
            parent = dir.parent();
        }

        if last_emit.elapsed() >= Duration::from_millis(450) {
            on_progress(AnalyzeProgress { scanned_files });
            last_emit = Instant::now();
        }
    }

    on_progress(AnalyzeProgress { scanned_files });

    let mut children_by_parent: HashMap<String, Vec<FolderUsage>> = HashMap::new();

    for (dir_path, size) in &dir_sizes {
        if dir_path == &root {
            continue;
        }

        let Some(parent) = Path::new(dir_path)
            .parent()
            .and_then(|v| v.to_str())
            .map(normalize_path)
        else {
            continue;
        };

        children_by_parent
            .entry(parent)
            .or_default()
            .push(FolderUsage {
                name: display_name(dir_path),
                path: dir_path.clone(),
                size: *size,
                is_dir: true,
            });
    }

    for (parent, mut files) in files_by_parent {
        children_by_parent
            .entry(parent)
            .or_default()
            .append(&mut files);
    }

    children_by_parent.entry(root).or_default();

    children_by_parent
}

fn average_ms_per_root(completed_ms_total: u64, completed_count: u32) -> Option<u64> {
    if completed_count == 0 {
        None
    } else {
        Some(completed_ms_total / u64::from(completed_count))
    }
}

fn estimate_eta_ms(
    completed_ms_total: u64,
    completed_count: u32,
    total_roots: u32,
    roots_done: u32,
) -> Option<u64> {
    let avg = average_ms_per_root(completed_ms_total, completed_count)?;
    Some(avg.saturating_mul(u64::from(total_roots.saturating_sub(roots_done))))
}

fn file_disk_usage_bytes(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        let allocated = meta.blocks().saturating_mul(512);
        if allocated > 0 {
            allocated
        } else {
            meta.len()
        }
    }

    #[cfg(not(unix))]
    {
        meta.len()
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn display_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.to_string())
}

fn system_scan_roots(mode: ScanMode) -> Vec<String> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        roots.push(normalize_path(&home));
    }
    if mode == ScanMode::Full {
        roots.push("/".to_string());
    }
    roots
}

fn should_skip_path(scan_root: &str, path: &Path) -> bool {
    if scan_root != "/" {
        return false;
    }

    let raw = path.to_string_lossy();
    let blocked = ["/proc", "/sys", "/dev", "/run", "/tmp"];
    blocked
        .iter()
        .any(|prefix| raw == *prefix || raw.starts_with(&format!("{prefix}/")))
}

fn sort_and_dedup_children(folder_usage: &mut HashMap<String, Vec<FolderUsage>>) {
    for children in folder_usage.values_mut() {
        children.sort_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| b.is_dir.cmp(&a.is_dir))
                .then_with(|| a.path.cmp(&b.path))
        });
        children.dedup_by(|a, b| a.path == b.path && a.is_dir == b.is_dir);
    }
}
