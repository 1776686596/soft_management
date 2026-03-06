use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use freedesktop_desktop_entry::DesktopEntry;
use once_cell::sync::Lazy;
use walkdir::WalkDir;

#[derive(Clone, Debug, Default)]
pub struct MemorySnapshot {
    pub mem_total: Option<u64>,
    pub mem_available: Option<u64>,
    pub swap_total: Option<u64>,
    pub swap_free: Option<u64>,
}

impl MemorySnapshot {
    pub fn mem_used(&self) -> Option<u64> {
        Some(self.mem_total?.saturating_sub(self.mem_available?))
    }

    pub fn swap_used(&self) -> Option<u64> {
        Some(self.swap_total?.saturating_sub(self.swap_free?))
    }
}

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub uid: u32,
    pub rss_bytes: Option<u64>,
    pub cmdline: Option<String>,
    pub icon_name: Option<String>,
}

pub struct ProcessScanEvent {
    pub scan_id: u64,
    pub memory: MemorySnapshot,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminateSignal {
    Term,
    Kill,
}

#[derive(Debug, thiserror::Error)]
pub enum TerminateError {
    #[error("permission denied")]
    PermissionDenied,
    #[error("refuse to terminate self process")]
    SelfProcess,
    #[error("process not found")]
    NotFound,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("system error: {0}")]
    System(String),
}

pub async fn scan_all(
    tx: async_channel::Sender<ProcessScanEvent>,
    token: tokio_util::sync::CancellationToken,
    scan_id: u64,
) {
    let token_for_worker = token.clone();
    let analyzed = tokio::task::spawn_blocking(move || scan_all_blocking(&token_for_worker))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("process scan worker failed: {e}");
            (MemorySnapshot::default(), Vec::new())
        });

    if token.is_cancelled() {
        return;
    }

    let (memory, processes) = analyzed;
    let event = ProcessScanEvent {
        scan_id,
        memory,
        processes,
    };
    let _ = tx.send(event).await;
}

pub fn read_memory_snapshot() -> MemorySnapshot {
    read_meminfo().unwrap_or_default()
}

fn scan_all_blocking(
    token: &tokio_util::sync::CancellationToken,
) -> (MemorySnapshot, Vec<ProcessInfo>) {
    let memory = read_meminfo().unwrap_or_default();
    // 预热桌面图标索引，避免在后续逻辑中首次构建导致额外抖动。
    let _ = &*PROCESS_ICON_INDEX;
    let processes = scan_processes(token);
    (memory, processes)
}

pub fn current_uid() -> u32 {
    // 安全边界：结束进程仅允许同 UID；这里取当前有效 UID。
    #[cfg(unix)]
    unsafe {
        libc::geteuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub fn self_pid() -> u32 {
    std::process::id()
}

pub fn can_terminate(current_uid: u32, self_pid: u32, info: &ProcessInfo) -> bool {
    info.pid != self_pid && info.uid == current_uid
}

pub fn terminate_process(
    pid: u32,
    signal: TerminateSignal,
    current_uid: u32,
    self_pid: u32,
) -> Result<(), TerminateError> {
    if pid == self_pid {
        return Err(TerminateError::SelfProcess);
    }

    let owner_uid = read_process_uid(pid)?;
    if owner_uid != current_uid {
        return Err(TerminateError::PermissionDenied);
    }

    let sig = match signal {
        TerminateSignal::Term => libc::SIGTERM,
        TerminateSignal::Kill => libc::SIGKILL,
    };

    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, sig) == 0 {
            return Ok(());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = sig;
        return Err(TerminateError::System("unsupported platform".into()));
    }

    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::EPERM => Err(TerminateError::PermissionDenied),
        Some(code) if code == libc::ESRCH => Err(TerminateError::NotFound),
        _ => Err(TerminateError::System(err.to_string())),
    }
}

fn read_meminfo() -> Option<MemorySnapshot> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    Some(parse_meminfo(&raw))
}

fn parse_meminfo(contents: &str) -> MemorySnapshot {
    let mut snapshot = MemorySnapshot::default();
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let key = key.trim_end_matches(':');
        let Some(value_str) = parts.next() else {
            continue;
        };
        let Ok(value) = value_str.parse::<u64>() else {
            continue;
        };
        let unit = parts.next().unwrap_or("");
        let bytes = match unit {
            "kB" => value.saturating_mul(1024),
            _ => value,
        };

        match key {
            "MemTotal" => snapshot.mem_total = Some(bytes),
            "MemAvailable" => snapshot.mem_available = Some(bytes),
            "SwapTotal" => snapshot.swap_total = Some(bytes),
            "SwapFree" => snapshot.swap_free = Some(bytes),
            _ => {}
        }
    }
    snapshot
}

fn scan_processes(token: &tokio_util::sync::CancellationToken) -> Vec<ProcessInfo> {
    let mut processes = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    let current_uid = current_uid();
    let mut desktop_icon_cache: HashMap<String, Option<String>> = HashMap::new();

    for entry in entries.filter_map(Result::ok) {
        if token.is_cancelled() {
            return Vec::new();
        }

        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };

        let status_path = proc_status_path(pid);
        let Ok(status_raw) = fs::read_to_string(&status_path) else {
            continue;
        };
        let Some(status) = parse_status(&status_raw) else {
            continue;
        };

        let cmdline = read_cmdline(pid).ok().and_then(normalize_cmdline);
        let exe = read_exe_path(pid);
        let icon_name = resolve_process_icon_name(
            pid,
            status.uid,
            current_uid,
            exe.as_deref(),
            cmdline.as_deref(),
            &status.name,
            &mut desktop_icon_cache,
        );

        processes.push(ProcessInfo {
            pid,
            name: status.name,
            uid: status.uid,
            rss_bytes: status.rss_bytes,
            cmdline,
            icon_name,
        });
    }

    processes.sort_by(|a, b| match (a.rss_bytes, b.rss_bytes) {
        (Some(a_s), Some(b_s)) => b_s.cmp(&a_s),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.pid.cmp(&b.pid),
    });

    processes
}

#[derive(Debug)]
struct ProcessStatus {
    name: String,
    uid: u32,
    rss_bytes: Option<u64>,
}

fn parse_status(contents: &str) -> Option<ProcessStatus> {
    let mut name: Option<String> = None;
    let mut uid: Option<u32> = None;
    let mut rss_kb: Option<u64> = None;

    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("Name:") {
            name = Some(rest.trim().to_string());
            continue;
        }

        if let Some(rest) = line.strip_prefix("Uid:") {
            uid = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u32>().ok());
            continue;
        }

        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let mut parts = rest.split_whitespace();
            rss_kb = parts.next().and_then(|v| v.parse::<u64>().ok());
        }
    }

    let name = name?;
    let uid = uid?;
    let rss_bytes = rss_kb.map(|v| v.saturating_mul(1024));

    Some(ProcessStatus {
        name,
        uid,
        rss_bytes,
    })
}

fn read_cmdline(pid: u32) -> std::io::Result<Vec<u8>> {
    fs::read(proc_cmdline_path(pid))
}

fn normalize_cmdline(raw: Vec<u8>) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&raw).into_owned();
    s = s.replace('\0', " ");
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn read_exe_path(pid: u32) -> Option<String> {
    let link = proc_pid_path(pid).join("exe");
    let path = fs::read_link(link).ok()?;
    let mut s = path.to_string_lossy().to_string();
    if let Some(trimmed) = s.strip_suffix(" (deleted)") {
        s = trimmed.to_string();
    }
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn read_process_uid(pid: u32) -> Result<u32, TerminateError> {
    let status_path = proc_status_path(pid);
    let status_raw = fs::read_to_string(&status_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => TerminateError::NotFound,
        _ => TerminateError::Io(e),
    })?;

    let status =
        parse_status(&status_raw).ok_or_else(|| TerminateError::System("bad status".into()))?;
    Ok(status.uid)
}

fn proc_status_path(pid: u32) -> PathBuf {
    proc_pid_path(pid).join("status")
}

fn proc_cmdline_path(pid: u32) -> PathBuf {
    proc_pid_path(pid).join("cmdline")
}

fn proc_pid_path(pid: u32) -> PathBuf {
    Path::new("/proc").join(pid.to_string())
}

#[derive(Debug, Default)]
struct ProcessIconIndex {
    // key 统一为小写；包含 exec 完整路径、exec basename、desktop file stem 等。
    icon_by_key: HashMap<String, String>,
    // Chrome PWA/应用：app-id -> icon name（通常为 chrome-<appId>-<profile>）
    chrome_app_icon_by_id: HashMap<String, String>,
}

static PROCESS_ICON_INDEX: Lazy<ProcessIconIndex> = Lazy::new(build_process_icon_index);

fn build_process_icon_index() -> ProcessIconIndex {
    let mut index = ProcessIconIndex::default();
    let mut ambiguous_keys: HashSet<String> = HashSet::new();
    let mut exec_resolve_cache: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut wrapper_target_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    for dir in desktop_dirs_for_icons() {
        if !dir.exists() {
            continue;
        }

        for entry in WalkDir::new(&dir).follow_links(false).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let is_desktop = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("desktop"));
            if !is_desktop {
                continue;
            }

            let de = match DesktopEntry::from_path(path, None::<&[&str]>) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if de.no_display() || de.hidden() {
                continue;
            }

            let Some(icon_raw) = de.icon().map(str::trim).filter(|v| !v.is_empty()) else {
                continue;
            };
            // desktop entry 允许 Icon=/abs/path；当前 UI 使用 icon_name 渲染，避免传入路径导致显示缺失。
            if icon_raw.contains('/') {
                continue;
            }
            let icon = normalize_icon_name(icon_raw);

            let exec_raw = de.exec().unwrap_or_default();

            // Chrome 生成的 PWA/应用（--app-id）会产生大量 desktop 文件。
            // 这些条目共享同一个 Exec（google-chrome wrapper），但 icon 各不相同；
            // 若把它们参与通用索引，会导致 Chrome 本体进程被错误映射成某个 PWA 的图标。
            if let Some(app_id) = extract_chrome_app_id(exec_raw) {
                index
                    .chrome_app_icon_by_id
                    .entry(app_id)
                    .or_insert_with(|| icon.clone());
                continue;
            }

            if let Some(exec_path) = extract_exec_path(exec_raw) {
                index_insert_key(&mut index, &mut ambiguous_keys, &exec_path, &icon);
                if let Some(base) = basename_key(&exec_path) {
                    index_insert_key(&mut index, &mut ambiguous_keys, &base, &icon);
                }

                // 额外补全：把 Exec 解析成绝对路径/真实目标二进制，解决 wrapper 与真实进程路径不一致的问题。
                if let Some(exec_abs) =
                    resolve_exec_token_to_path(&exec_path, &mut exec_resolve_cache)
                {
                    index_insert_key(
                        &mut index,
                        &mut ambiguous_keys,
                        exec_abs.to_string_lossy().as_ref(),
                        &icon,
                    );

                    if let Ok(canon) = fs::canonicalize(&exec_abs) {
                        index_insert_key(
                            &mut index,
                            &mut ambiguous_keys,
                            canon.to_string_lossy().as_ref(),
                            &icon,
                        );

                        for target in wrapper_target_paths(&canon, &mut wrapper_target_cache) {
                            index_insert_key(
                                &mut index,
                                &mut ambiguous_keys,
                                target.to_string_lossy().as_ref(),
                                &icon,
                            );
                        }
                    }
                }
            }

            if let Some(stem) = path.file_stem().and_then(|v| v.to_str()) {
                if !stem.trim().is_empty() {
                    index_insert_key(&mut index, &mut ambiguous_keys, stem, &icon);
                }
            }
        }
    }

    index
}

fn desktop_dirs_for_icons() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        // snap 的 desktop 文件通常位于此处
        PathBuf::from("/var/lib/snapd/desktop/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];

    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".local/share/applications"));
    }

    dirs
}

fn index_insert_key(
    index: &mut ProcessIconIndex,
    ambiguous_keys: &mut HashSet<String>,
    raw_key: &str,
    icon: &str,
) {
    let Some(key) = normalize_icon_key(raw_key) else {
        return;
    };
    if ambiguous_keys.contains(&key) {
        return;
    }

    match index.icon_by_key.get(&key) {
        None => {
            index.icon_by_key.insert(key, icon.to_string());
        }
        Some(existing) if existing == icon => {}
        Some(_) => {
            index.icon_by_key.remove(&key);
            ambiguous_keys.insert(key);
        }
    }
}

fn normalize_icon_key(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.to_ascii_lowercase())
}

fn normalize_icon_name(input: &str) -> String {
    let trimmed = input.trim();
    for ext in [".png", ".svg", ".xpm"] {
        if trimmed.ends_with(ext) {
            return trimmed[..trimmed.len().saturating_sub(ext.len())].to_string();
        }
    }
    trimmed.to_string()
}

fn basename_key(path_or_name: &str) -> Option<String> {
    let trimmed = path_or_name.trim_matches('"').trim_matches('\'').trim();
    if trimmed.is_empty() {
        return None;
    }
    let file_name = Path::new(trimmed)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or(trimmed);
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or(file_name);
    let stem = stem.trim();
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

fn extract_exec_path(exec: &str) -> Option<String> {
    let mut tokens = exec.split_whitespace();
    let mut candidate = tokens.next()?;
    if candidate == "env" || candidate.contains('=') {
        candidate = tokens.find(|t| !t.contains('=') && !t.starts_with('%'))?;
    }
    let cleaned = candidate.trim_matches('"').trim_matches('\'');
    if cleaned.is_empty() || cleaned.starts_with('%') {
        return None;
    }
    Some(cleaned.to_string())
}

fn extract_chrome_app_id(raw: &str) -> Option<String> {
    // 常见形式：--app-id=anajjmnhfmakkamckgeopokbjfkinihm
    if let Some(pos) = raw.find("--app-id=") {
        let tail = &raw[pos + "--app-id=".len()..];
        let id: String = tail
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect();
        return normalize_chrome_app_id(&id);
    }

    // 兼容：--app-id anaj...
    if let Some(pos) = raw.find("--app-id") {
        let tail = &raw[pos + "--app-id".len()..];
        let tail = tail.trim_start();
        let id: String = tail
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect();
        return normalize_chrome_app_id(&id);
    }

    None
}

fn normalize_chrome_app_id(raw: &str) -> Option<String> {
    let id = raw.trim().to_ascii_lowercase();
    if id.len() == 32 && id.chars().all(|ch| ch.is_ascii_lowercase()) {
        Some(id)
    } else {
        None
    }
}

fn resolve_exec_token_to_path(
    exec_token: &str,
    cache: &mut HashMap<String, Option<PathBuf>>,
) -> Option<PathBuf> {
    if let Some(cached) = cache.get(exec_token) {
        return cached.clone();
    }

    let resolved = if exec_token.contains('/') {
        let p = PathBuf::from(exec_token);
        if p.is_absolute() && p.is_file() {
            Some(p)
        } else {
            None
        }
    } else {
        resolve_in_path(exec_token)
    };

    cache.insert(exec_token.to_string(), resolved.clone());
    resolved
}

fn resolve_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':').map(str::trim).filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn wrapper_target_paths(
    script_path: &Path,
    cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
) -> Vec<PathBuf> {
    if let Some(cached) = cache.get(script_path) {
        return cached.clone();
    }

    let mut out: Vec<PathBuf> = Vec::new();
    let raw = read_file_limited(script_path, 64 * 1024).unwrap_or_default();
    if !raw.starts_with(b"#!") {
        cache.insert(script_path.to_path_buf(), out.clone());
        return out;
    }

    let Some(dir) = script_path.parent() else {
        cache.insert(script_path.to_path_buf(), out.clone());
        return out;
    };

    let text = String::from_utf8_lossy(&raw);
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for prefix in ["$HERE/", "${HERE}/"] {
        for (idx, _) in text.match_indices(prefix) {
            let tail = &text[idx + prefix.len()..];
            let token: String = tail
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '+'))
                .collect();
            if token.is_empty() {
                continue;
            }
            let candidate = dir.join(&token);
            if !candidate.is_file() {
                continue;
            }
            if seen.insert(candidate.clone()) {
                out.push(candidate);
            }
            if out.len() >= 6 {
                break;
            }
        }
    }

    cache.insert(script_path.to_path_buf(), out.clone());
    out
}

fn resolve_process_icon_name(
    pid: u32,
    owner_uid: u32,
    current_uid: u32,
    exe: Option<&str>,
    cmdline: Option<&str>,
    name: &str,
    desktop_icon_cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    // Chrome PWA：renderer/utility 等子进程的 cmdline 会携带 --app-id，优先用它映射到正确图标。
    if let Some(cmdline) = cmdline {
        if let Some(app_id) = extract_chrome_app_id(cmdline) {
            let index = &*PROCESS_ICON_INDEX;
            if let Some(icon) = index.chrome_app_icon_by_id.get(&app_id) {
                return Some(icon.clone());
            }
            // 兜底：Chrome 的桌面图标常用该命名规则。
            return Some(format!("chrome-{app_id}-Default"));
        }
    }

    let icon = resolve_process_icon_name_from_index(exe, cmdline, name);
    if icon.is_some() {
        return icon;
    }

    if owner_uid == current_uid {
        return resolve_process_icon_name_from_environ(pid, desktop_icon_cache);
    }

    None
}

fn resolve_process_icon_name_from_index(
    exe: Option<&str>,
    cmdline: Option<&str>,
    name: &str,
) -> Option<String> {
    let index = &*PROCESS_ICON_INDEX;

    let mut candidates: Vec<String> = Vec::new();
    push_icon_candidates(&mut candidates, exe);

    if let Some(cmdline) = cmdline {
        if let Some(first) = cmdline.split_whitespace().next() {
            push_icon_candidates(&mut candidates, Some(first));
        }
    }

    if let Some(key) = normalize_icon_key(name) {
        candidates.push(key);
    }

    for key in candidates {
        if let Some(icon) = index.icon_by_key.get(&key) {
            return Some(icon.clone());
        }
    }

    None
}

fn push_icon_candidates(out: &mut Vec<String>, raw: Option<&str>) {
    let Some(raw) = raw else {
        return;
    };

    let base = basename_key(raw);
    if base.as_deref().is_some_and(is_generic_exec_name) {
        return;
    }

    if let Some(key) = normalize_icon_key(raw) {
        out.push(key);
    }
    if let Some(base) = base.and_then(|v| normalize_icon_key(&v)) {
        out.push(base);
    }
}

fn is_generic_exec_name(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "env"
            | "sh"
            | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "python"
            | "python2"
            | "python3"
            | "node"
            | "nodejs"
            | "java"
            | "javaw"
            | "mono"
            | "dotnet"
            | "sudo"
            | "pkexec"
    )
}

fn resolve_process_icon_name_from_environ(
    pid: u32,
    desktop_icon_cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    let raw = read_process_environ(pid)?;

    if let Some(hint) = environ_get(&raw, "GIO_LAUNCHED_DESKTOP_FILE")
        .or_else(|| environ_get(&raw, "BAMF_DESKTOP_FILE_HINT"))
    {
        if let Some(icon) = resolve_icon_from_desktop_hint(&hint, desktop_icon_cache) {
            return Some(icon);
        }
    }

    if let Some(app_id) =
        environ_get(&raw, "FLATPAK_ID").or_else(|| environ_get(&raw, "FLATPAK_APPID"))
    {
        let app_id = app_id.trim();
        if !app_id.is_empty() && !app_id.contains('/') {
            let index = &*PROCESS_ICON_INDEX;
            if let Some(key) = normalize_icon_key(app_id) {
                if let Some(icon) = index.icon_by_key.get(&key) {
                    return Some(icon.clone());
                }
            }
            // flatpak 通常以 app_id 作为图标名安装到主题中；即使索引未命中也值得尝试。
            return Some(app_id.to_string());
        }
    }

    None
}

fn read_process_environ(pid: u32) -> Option<Vec<u8>> {
    let path = proc_pid_path(pid).join("environ");
    read_file_limited(&path, 64 * 1024)
}

fn read_file_limited(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    let _ = file
        .take(u64::try_from(max_bytes).unwrap_or(u64::MAX))
        .read_to_end(&mut buf)
        .ok()?;
    if buf.is_empty() {
        None
    } else {
        Some(buf)
    }
}

fn environ_get(raw: &[u8], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    let prefix = prefix.as_bytes();
    for part in raw.split(|b| *b == 0) {
        if part.starts_with(prefix) {
            let value = &part[prefix.len()..];
            let value = String::from_utf8_lossy(value).trim().to_string();
            return (!value.is_empty()).then_some(value);
        }
    }
    None
}

fn resolve_icon_from_desktop_hint(
    hint_raw: &str,
    desktop_icon_cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    let hint = hint_raw.trim().trim_matches('"').trim_matches('\'').trim();
    if hint.is_empty() {
        return None;
    }

    let hint = hint.strip_prefix("file://").unwrap_or(hint);
    if hint.contains('/') {
        return resolve_icon_from_desktop_file_path(hint, desktop_icon_cache);
    }

    let stem = hint.trim_end_matches(".desktop").trim();
    if stem.is_empty() {
        return None;
    }

    let index = &*PROCESS_ICON_INDEX;
    let key = normalize_icon_key(stem)?;
    index.icon_by_key.get(&key).cloned()
}

fn resolve_icon_from_desktop_file_path(
    path_raw: &str,
    desktop_icon_cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    let path = path_raw.trim();
    if path.is_empty() {
        return None;
    }

    if let Some(cached) = desktop_icon_cache.get(path) {
        return cached.clone();
    }

    let icon = DesktopEntry::from_path(Path::new(path), None::<&[&str]>)
        .ok()
        .and_then(|de| {
            let icon_raw = de.icon().map(str::trim).filter(|v| !v.is_empty())?;
            // 这里返回 icon name（主题内的名字），避免 UI 侧加载文件路径。
            (!icon_raw.contains('/')).then(|| normalize_icon_name(icon_raw))
        });

    desktop_icon_cache.insert(path.to_string(), icon.clone());
    icon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meminfo_extracts_fields() {
        let raw = r#"
MemTotal:       16384256 kB
MemFree:         1000000 kB
MemAvailable:    8000000 kB
SwapTotal:       2097148 kB
SwapFree:        1048574 kB
"#;
        let s = parse_meminfo(raw);
        assert_eq!(s.mem_total, Some(16_384_256u64 * 1024));
        assert_eq!(s.mem_available, Some(8_000_000u64 * 1024));
        assert_eq!(s.swap_total, Some(2_097_148u64 * 1024));
        assert_eq!(s.swap_free, Some(1_048_574u64 * 1024));
        assert_eq!(s.mem_used(), Some((16_384_256u64 - 8_000_000u64) * 1024));
        assert_eq!(s.swap_used(), Some((2_097_148u64 - 1_048_574u64) * 1024));
    }

    #[test]
    fn parse_status_extracts_name_uid_and_rss() {
        let raw = r#"
Name:   bash
Umask:  0022
State:  S (sleeping)
Uid:    1000    1000    1000    1000
VmRSS:    12345 kB
"#;
        let s = parse_status(raw).expect("status");
        assert_eq!(s.name, "bash");
        assert_eq!(s.uid, 1000);
        assert_eq!(s.rss_bytes, Some(12_345u64 * 1024));
    }

    #[test]
    fn can_terminate_requires_same_uid_and_not_self() {
        let info = ProcessInfo {
            pid: 123,
            name: "x".into(),
            uid: 1000,
            rss_bytes: None,
            cmdline: None,
            icon_name: None,
        };
        assert!(!can_terminate(1000, 123, &info));
        assert!(!can_terminate(1001, 999, &info));
        assert!(can_terminate(1000, 999, &info));
    }

    #[test]
    fn normalize_cmdline_splits_nul() {
        let raw = b"/usr/bin/python3\0-m\0http.server\0".to_vec();
        let s = normalize_cmdline(raw).expect("cmdline");
        assert_eq!(s, "/usr/bin/python3 -m http.server");
    }

    #[test]
    fn extract_exec_path_skips_env_and_assignments() {
        assert_eq!(
            extract_exec_path("env FOO=bar /usr/bin/firefox %u"),
            Some("/usr/bin/firefox".to_string())
        );
        assert_eq!(
            extract_exec_path("FOO=bar BAR=baz firefox --new-window"),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn basename_key_extracts_stem() {
        assert_eq!(
            basename_key("/usr/bin/python3.12"),
            Some("python3".to_string())
        );
        assert_eq!(basename_key("firefox"), Some("firefox".to_string()));
        assert_eq!(basename_key(""), None);
    }

    #[test]
    fn index_insert_key_drops_ambiguous_keys() {
        let mut index = ProcessIconIndex::default();
        let mut ambiguous: HashSet<String> = HashSet::new();

        index_insert_key(&mut index, &mut ambiguous, "python3", "idle");
        assert_eq!(index.icon_by_key.get("python3"), Some(&"idle".to_string()));

        // 同一个 key 出现不同 icon 时，认为歧义，移除并禁止后续写入。
        index_insert_key(&mut index, &mut ambiguous, "python3", "other");
        assert!(!index.icon_by_key.contains_key("python3"));
        assert!(ambiguous.contains("python3"));

        index_insert_key(&mut index, &mut ambiguous, "python3", "third");
        assert!(!index.icon_by_key.contains_key("python3"));
    }

    #[test]
    fn environ_get_parses_nul_separated_pairs() {
        let raw = b"FOO=bar\0GIO_LAUNCHED_DESKTOP_FILE=firefox.desktop\0EMPTY=\0";
        assert_eq!(environ_get(raw, "FOO"), Some("bar".to_string()));
        assert_eq!(
            environ_get(raw, "GIO_LAUNCHED_DESKTOP_FILE"),
            Some("firefox.desktop".to_string())
        );
        assert_eq!(environ_get(raw, "EMPTY"), None);
        assert_eq!(environ_get(raw, "MISSING"), None);
    }

    #[test]
    fn push_icon_candidates_skips_generic_exec_names() {
        let mut out = Vec::new();
        push_icon_candidates(&mut out, Some("/usr/bin/python3"));
        push_icon_candidates(&mut out, Some("java"));
        assert!(out.is_empty());

        push_icon_candidates(&mut out, Some("/usr/bin/firefox"));
        assert!(!out.is_empty());
    }

    #[test]
    fn extract_chrome_app_id_parses_common_forms() {
        assert_eq!(
            extract_chrome_app_id(
                "/opt/google/chrome/chrome --app-id=anajjmnhfmakkamckgeopokbjfkinihm"
            ),
            Some("anajjmnhfmakkamckgeopokbjfkinihm".to_string())
        );
        assert_eq!(
            extract_chrome_app_id(
                "google-chrome --app-id anajjmnhfmakkamckgeopokbjfkinihm --foo"
            ),
            Some("anajjmnhfmakkamckgeopokbjfkinihm".to_string())
        );
        assert_eq!(extract_chrome_app_id("google-chrome --app-id=bad"), None);
    }
}
