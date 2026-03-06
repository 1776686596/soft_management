pub struct Package {
    pub canonical_id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub install_method: String,
    pub install_path: Option<String>,
    pub uninstall_command: Option<String>,
    pub size: Option<u64>,
    pub description: String,
    pub icon_name: Option<String>,
    pub desktop_file: Option<String>,
}

pub struct AdapterResult<T> {
    pub items: Vec<T>,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
    pub timestamp: f64,
}

pub struct RuntimeInfo {
    pub language: String,
    pub version: String,
    pub path: String,
    pub install_method: String,
}

pub struct VersionManagerInfo {
    pub name: String,
    pub managed_versions: Vec<ManagedVersion>,
    pub path: String,
}

pub struct ManagedVersion {
    pub version: String,
    pub active: bool,
}

pub struct GlobalPackageInfo {
    pub manager: String,
    pub name: String,
    pub version: String,
}

#[derive(Clone)]
pub struct CacheInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub requires_sudo: bool,
}

#[derive(Clone)]
pub struct CleanupSuggestion {
    pub description: String,
    pub targets: Vec<String>,
    pub estimated_bytes: u64,
    pub command: String,
    pub requires_sudo: bool,
    pub risk_level: RiskLevel,
}

#[derive(Clone, Copy)]
pub enum RiskLevel {
    Safe,
    Moderate,
}

const CLEANUP_RUN_WHITELIST: &[&str] = &[
    "pip3 cache purge",
    "npm cache clean --force",
    "conda clean --all -y",
    "cargo cache --autoclean",
    "docker system prune -f",
];

const CLEANUP_COPY_WHITELIST: &[&str] = &[
    "apt clean",
    "apt autoremove --purge",
    "journalctl --vacuum-time=7d",
    "journalctl --vacuum-size=200M",
    "docker system prune -a --volumes",
];

fn cleanup_command_allowed(command: &str, requires_sudo: bool) -> bool {
    if requires_sudo {
        if CLEANUP_COPY_WHITELIST.contains(&command) {
            return true;
        }
        return cleanup_command_matches_patterns(command);
    }

    CLEANUP_RUN_WHITELIST.contains(&command)
}

fn cleanup_command_matches_patterns(command: &str) -> bool {
    let parts: Vec<&str> = command.split_whitespace().collect();
    is_allowed_snap_remove(&parts) || is_allowed_truncate_var_log(&parts)
}

fn is_allowed_snap_remove(parts: &[&str]) -> bool {
    if parts.len() != 5 {
        return false;
    }
    if parts[0] != "snap" || parts[1] != "remove" || parts[3] != "--revision" {
        return false;
    }
    let name = parts[2];
    let rev = parts[4];
    if !rev.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return false;
    }
    true
}

fn is_allowed_truncate_var_log(parts: &[&str]) -> bool {
    if parts.len() != 4 {
        return false;
    }
    if parts[0] != "truncate" || parts[1] != "-s" || parts[2] != "0" {
        return false;
    }
    let path = parts[3];
    if !path.starts_with("/var/log/") || path.starts_with("/var/log/journal") {
        return false;
    }
    if path.contains("..") || path.len() > 512 {
        return false;
    }

    path.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || ch == '/'
            || ch == '.'
            || ch == '-'
            || ch == '_'
            || ch == '@'
            || ch == '+'
    })
}

impl CleanupSuggestion {
    pub fn new(
        description: String,
        estimated_bytes: u64,
        command: String,
        requires_sudo: bool,
        risk_level: RiskLevel,
    ) -> Option<Self> {
        if !cleanup_command_allowed(&command, requires_sudo) {
            tracing::warn!("cleanup command not in whitelist: {command}");
            return None;
        }
        Some(Self {
            description,
            targets: Vec::new(),
            estimated_bytes,
            command,
            requires_sudo,
            risk_level,
        })
    }
}

pub fn make_canonical_id(source: &str, name: &str) -> String {
    format!("{source}:{name}")
}

pub fn parse_canonical_id(id: &str) -> (&str, &str) {
    let mut parts = id.splitn(2, ':');
    let source = parts.next().expect("canonical_id missing source");
    let name = parts.next().expect("canonical_id missing name");
    (source, name)
}

pub fn detect_install_method(path: &str) -> &'static str {
    if path.contains("/.nvm/") {
        "nvm"
    } else if path.contains("/.rustup/") {
        "rustup"
    } else if path.contains("/anaconda3/") || path.contains("/miniconda3/") {
        "conda"
    } else if path.contains("/.cargo/bin/") {
        "cargo"
    } else if path.starts_with("/usr/local/bin/") {
        "manual"
    } else if path.starts_with("/usr/bin/") || path.starts_with("/bin/") {
        "apt"
    } else if path.contains("/.local/bin/") {
        "pipx"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_id_roundtrip() {
        let id = make_canonical_id("apt", "vim");
        assert_eq!(id, "apt:vim");
        let (source, name) = parse_canonical_id(&id);
        assert_eq!(source, "apt");
        assert_eq!(name, "vim");
    }

    #[test]
    fn canonical_id_with_colon_in_name() {
        let id = make_canonical_id("apt", "libc6:amd64");
        assert_eq!(id, "apt:libc6:amd64");
        let (source, name) = parse_canonical_id(&id);
        assert_eq!(source, "apt");
        assert_eq!(name, "libc6:amd64");
    }

    #[test]
    fn install_method_detection() {
        assert_eq!(
            detect_install_method("/home/u/.nvm/versions/node/v20/bin/node"),
            "nvm"
        );
        assert_eq!(
            detect_install_method("/home/u/.rustup/toolchains/stable/bin/rustc"),
            "rustup"
        );
        assert_eq!(
            detect_install_method("/home/u/anaconda3/bin/python"),
            "conda"
        );
        assert_eq!(detect_install_method("/home/u/.cargo/bin/cargo"), "cargo");
        assert_eq!(detect_install_method("/usr/local/bin/myapp"), "manual");
        assert_eq!(detect_install_method("/usr/bin/python3"), "apt");
        assert_eq!(detect_install_method("/home/u/.local/bin/pipx"), "pipx");
        assert_eq!(detect_install_method("/opt/custom/bin/tool"), "unknown");
    }

    #[test]
    fn cleanup_whitelist_accepts_valid() {
        assert!(
            CleanupSuggestion::new("t".into(), 1, "apt clean".into(), true, RiskLevel::Safe)
                .is_some()
        );
        assert!(CleanupSuggestion::new(
            "t".into(),
            1,
            "pip3 cache purge".into(),
            false,
            RiskLevel::Safe
        )
        .is_some());
        assert!(CleanupSuggestion::new(
            "t".into(),
            1,
            "docker system prune -f".into(),
            false,
            RiskLevel::Moderate
        )
        .is_some());
        assert!(CleanupSuggestion::new(
            "t".into(),
            1,
            "snap remove cmake --revision 1070".into(),
            true,
            RiskLevel::Moderate
        )
        .is_some());
        assert!(CleanupSuggestion::new(
            "t".into(),
            1,
            "truncate -s 0 /var/log/syslog".into(),
            true,
            RiskLevel::Moderate
        )
        .is_some());
    }

    #[test]
    fn cleanup_whitelist_rejects_invalid() {
        assert!(
            CleanupSuggestion::new("t".into(), 1, "rm -rf /".into(), false, RiskLevel::Safe)
                .is_none()
        );
        assert!(CleanupSuggestion::new(
            "t".into(),
            1,
            "snap remove cmake --revision 1070".into(),
            false,
            RiskLevel::Moderate
        )
        .is_none());
        assert!(CleanupSuggestion::new(
            "t".into(),
            1,
            "truncate -s 0 /etc/passwd".into(),
            true,
            RiskLevel::Moderate
        )
        .is_none());
    }

    #[test]
    fn sort_packages_by_size() {
        let pkgs = vec![
            Package {
                canonical_id: "a:1".into(),
                name: "a".into(),
                version: String::new(),
                source: "a".into(),
                install_method: "a".into(),
                install_path: None,
                uninstall_command: None,
                size: Some(100),
                description: String::new(),
                icon_name: None,
                desktop_file: None,
            },
            Package {
                canonical_id: "a:2".into(),
                name: "b".into(),
                version: String::new(),
                source: "a".into(),
                install_method: "a".into(),
                install_path: None,
                uninstall_command: None,
                size: Some(500),
                description: String::new(),
                icon_name: None,
                desktop_file: None,
            },
            Package {
                canonical_id: "a:3".into(),
                name: "c".into(),
                version: String::new(),
                source: "a".into(),
                install_method: "a".into(),
                install_path: None,
                uninstall_command: None,
                size: None,
                description: String::new(),
                icon_name: None,
                desktop_file: None,
            },
        ];
        let mut sorted: Vec<&Package> = pkgs.iter().collect();
        sorted.sort_by(|a, b| match (a.size, b.size) {
            (Some(a_s), Some(b_s)) => b_s.cmp(&a_s),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        assert_eq!(sorted[0].name, "b");
        assert_eq!(sorted[1].name, "a");
        assert_eq!(sorted[2].name, "c");
    }
}
