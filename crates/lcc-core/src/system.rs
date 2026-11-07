//! Facts about the running system: distribution, kernel, desktop and which
//! package managers are installed.

use std::collections::HashMap;
use std::path::Path;

/// A package manager the app knows how to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackageManager {
    Pacman,
    Apt,
    Dnf,
    Zypper,
    Flatpak,
    Snap,
}

impl PackageManager {
    pub const ALL: [PackageManager; 6] = [
        Self::Pacman,
        Self::Apt,
        Self::Dnf,
        Self::Zypper,
        Self::Flatpak,
        Self::Snap,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Pacman => "pacman",
            Self::Apt => "APT",
            Self::Dnf => "DNF",
            Self::Zypper => "Zypper",
            Self::Flatpak => "Flatpak",
            Self::Snap => "Snap",
        }
    }

    /// The program whose presence means this package manager is installed.
    fn program(self) -> &'static str {
        match self {
            Self::Pacman => "pacman",
            Self::Apt => "apt-get",
            Self::Dnf => "dnf",
            Self::Zypper => "zypper",
            Self::Flatpak => "flatpak",
            Self::Snap => "snap",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    /// e.g. "CachyOS Linux"
    pub distro_name: String,
    /// e.g. "cachyos"
    pub distro_id: String,
    /// Parent distributions, e.g. ["arch"]
    pub distro_like: Vec<String>,
    /// e.g. "7.3.1-1-cachyos"
    pub kernel: String,
    /// e.g. "GNOME", "KDE", "niri"
    pub desktop: Option<String>,
    pub package_managers: Vec<PackageManager>,
}

impl SystemInfo {
    /// Reads the current system. Never fails; unknown values are left empty.
    pub fn detect() -> Self {
        let os = ["/etc/os-release", "/usr/lib/os-release"]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .map(|t| parse_os_release(&t))
            .unwrap_or_default();

        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();

        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .ok()
            .or_else(|| std::env::var("DESKTOP_SESSION").ok())
            .filter(|s| !s.is_empty());

        let path = std::env::var_os("PATH").unwrap_or_default();
        let package_managers = PackageManager::ALL
            .into_iter()
            .filter(|pm| program_in_path(pm.program(), &path))
            .collect();

        Self {
            distro_name: os
                .get("PRETTY_NAME")
                .or_else(|| os.get("NAME"))
                .cloned()
                .unwrap_or_else(|| "Linux".to_owned()),
            distro_id: os.get("ID").cloned().unwrap_or_default(),
            distro_like: os
                .get("ID_LIKE")
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            kernel,
            desktop,
            package_managers,
        }
    }

    pub fn has(&self, pm: PackageManager) -> bool {
        self.package_managers.contains(&pm)
    }
}

fn program_in_path(program: &str, path: &std::ffi::OsStr) -> bool {
    std::env::split_paths(path).any(|dir| is_executable(&dir.join(program)))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Parses the `KEY=value` format of `/etc/os-release`.
pub fn parse_os_release(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), unquote(v.trim())))
        .collect()
}

fn unquote(v: &str) -> String {
    let v = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v);
    v.replace("\\\"", "\"").replace("\\\\", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_os_release() {
        let text = r#"
NAME="CachyOS Linux"
PRETTY_NAME="CachyOS"
ID=cachyos
ID_LIKE=arch
# comment
HOME_URL='https://cachyos.org'
"#;
        let os = parse_os_release(text);
        assert_eq!(os["NAME"], "CachyOS Linux");
        assert_eq!(os["PRETTY_NAME"], "CachyOS");
        assert_eq!(os["ID"], "cachyos");
        assert_eq!(os["ID_LIKE"], "arch");
        assert_eq!(os["HOME_URL"], "https://cachyos.org");
        assert!(!os.contains_key("# comment"));
    }

    #[test]
    fn finds_programs_in_path() {
        let path = std::ffi::OsString::from("/nonexistent:/usr/bin:/bin");
        assert!(program_in_path("sh", &path));
        assert!(!program_in_path("definitely-not-a-real-program-xyz", &path));
    }

    #[test]
    fn detect_does_not_panic() {
        let info = SystemInfo::detect();
        assert!(!info.distro_name.is_empty());
    }
}
