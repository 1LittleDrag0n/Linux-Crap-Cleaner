//! Installed software: pacman packages, Flatpaks and AppImages.
//!
//! pacman's local database (`/var/lib/pacman/local`) is read directly, which
//! is fast and gives exact sizes, install dates and whether a package was
//! installed on purpose or as a dependency. Flatpaks come from `flatpak list`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::desktop::{self, DesktopEntry};
use crate::native::Backend;

/// Where a piece of software came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// The system package manager.
    Native(Backend),
    Flatpak,
    Snap,
    AppImage,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Native(b) => b.name(),
            Source::Flatpak => "Flatpak",
            Source::Snap => "Snap",
            Source::AppImage => "AppImage",
        }
    }
}

/// One installed package or app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Package name, Flatpak app id, or AppImage file name.
    pub name: String,
    /// Friendly name from the app's launcher, or the package name.
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub source: Source,
    /// Bytes on disk.
    pub size: u64,
    /// Unix time of installation, when known.
    pub installed: Option<i64>,
    /// Installed on purpose (not only as a dependency).
    pub explicit: bool,
    /// Has a launcher in the app menu.
    pub is_app: bool,
    /// Icon name or absolute icon path from the launcher.
    pub icon: Option<String>,
    /// AppImage file.
    pub path: Option<PathBuf>,
    /// Flatpak installation: "system" or "user".
    pub installation: Option<String>,
    /// Why this package may not be removed, if it may not.
    pub protected: Option<&'static str>,
}

/// One package from pacman's local database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PacmanEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub size: u64,
    pub install_date: Option<i64>,
    /// Installed as a dependency of another package.
    pub as_dependency: bool,
    pub depends: Vec<String>,
    /// Optional dependencies, as `name: why` lines.
    pub optdepends: Vec<String>,
    pub provides: Vec<String>,
}

/// Parses a `desc` file from pacman's local database.
pub fn parse_pacman_desc(text: &str) -> PacmanEntry {
    let mut entry = PacmanEntry::default();
    let mut section = "";
    for line in text.lines() {
        if line.starts_with('%') && line.ends_with('%') {
            section = line;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        match section {
            "%NAME%" => entry.name = line.to_owned(),
            "%VERSION%" => entry.version = line.to_owned(),
            "%DESC%" => entry.description = line.to_owned(),
            "%SIZE%" => entry.size = line.parse().unwrap_or(0),
            "%INSTALLDATE%" => entry.install_date = line.parse().ok(),
            "%REASON%" => entry.as_dependency = line == "1",
            "%DEPENDS%" => entry.depends.push(line.to_owned()),
            "%OPTDEPENDS%" => entry.optdepends.push(line.to_owned()),
            "%PROVIDES%" => entry.provides.push(line.to_owned()),
            _ => {}
        }
    }
    entry
}

pub const PACMAN_DB: &str = "/var/lib/pacman/local";

/// Reads every package from a pacman local database folder.
pub fn read_pacman_db(db: &Path) -> Vec<PacmanEntry> {
    let Ok(read) = std::fs::read_dir(db) else {
        return Vec::new();
    };
    let dirs: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    let mut entries: Vec<PacmanEntry> = dirs
        .par_iter()
        .filter_map(|d| std::fs::read_to_string(d.join("desc")).ok())
        .map(|t| parse_pacman_desc(&t))
        .filter(|e| !e.name.is_empty())
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// How well a launcher represents a package (lower is better): a launcher
/// named after the package first, then the shortest name, so
/// `libreoffice-fresh` shows as "LibreOffice" rather than "LibreOffice Base".
fn launcher_rank(package: &str, entry: &DesktopEntry) -> (u8, usize, String) {
    let id = entry.id.to_lowercase();
    let base = package.split('-').next().unwrap_or(package);
    let closeness = if id == package {
        0
    } else if id.ends_with(&format!(".{base}")) || id == base {
        1
    } else {
        2
    };
    (closeness, entry.name.len(), entry.id.clone())
}

/// Strips a version constraint or description: `foo>=1.2` → `foo`,
/// `libfoo.so=3-64` → `libfoo.so`, `sane: for scanning` → `sane`.
fn bare_name(dep: &str) -> &str {
    dep.split(['<', '>', '=', ':']).next().unwrap_or(dep).trim()
}

/// Packages installed as dependencies that nothing needs any more, exactly
/// like `pacman -Qdt`: a package another one lists as an optional dependency
/// still counts as needed.
pub fn pacman_orphans(entries: &[PacmanEntry]) -> Vec<&PacmanEntry> {
    let required: HashSet<&str> = entries
        .iter()
        .flat_map(|e| e.depends.iter().chain(&e.optdepends).map(|d| bare_name(d)))
        .collect();
    entries
        .iter()
        .filter(|e| e.as_dependency)
        .filter(|e| {
            !required.contains(e.name.as_str())
                && !e.provides.iter().any(|p| required.contains(bare_name(p)))
        })
        .collect()
}

/// Turns "12.3 MB" / "1.0 GB" / "512 bytes" (as printed by Flatpak) into
/// bytes.
pub fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim().replace('\u{a0}', " ");
    let (number, unit) = s.split_once(' ').unwrap_or((&s, "B"));
    let value: f64 = number.replace(',', ".").parse().ok()?;
    let factor: f64 = match unit.trim() {
        "B" | "bytes" | "byte" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * factor).round() as u64)
}

/// One line of `flatpak list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatpakRef {
    pub id: String,
    pub name: String,
    pub version: String,
    pub branch: String,
    pub arch: String,
    pub installation: String,
    pub size: u64,
    /// For apps: the runtime they use, e.g. `org.gnome.Platform/x86_64/47`.
    pub runtime: String,
}

const FLATPAK_COLUMNS: &str = "application,name,version,branch,arch,installation,size,runtime";

/// Parses tab-separated `flatpak list --columns=...` output.
pub fn parse_flatpak_list(text: &str) -> Vec<FlatpakRef> {
    text.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 8 || cols[0].is_empty() {
                return None;
            }
            Some(FlatpakRef {
                id: cols[0].to_owned(),
                name: cols[1].to_owned(),
                version: cols[2].to_owned(),
                branch: cols[3].to_owned(),
                arch: cols[4].to_owned(),
                installation: cols[5].to_owned(),
                size: parse_human_size(cols[6]).unwrap_or(0),
                runtime: cols[7].to_owned(),
            })
        })
        .collect()
}

/// Lists Flatpak apps (`runtimes = false`) or runtimes. Empty if Flatpak
/// isn't installed.
pub fn flatpak_list(runtimes: bool) -> Vec<FlatpakRef> {
    let kind = if runtimes { "--runtime" } else { "--app" };
    let output = crate::host::command("flatpak")
        .args(["list", kind, &format!("--columns={FLATPAK_COLUMNS}")])
        .env("LC_ALL", "C")
        .output();
    match output {
        Ok(o) if o.status.success() => parse_flatpak_list(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// Folders where people usually keep AppImages.
pub fn appimage_dirs(home: &Path) -> Vec<PathBuf> {
    [
        "Applications",
        ".local/bin",
        "AppImages",
        ".local/share/AppImage",
        "Downloads",
        "Desktop",
    ]
    .iter()
    .map(|d| home.join(d))
    .collect()
}

/// AppImage files directly inside `dirs`.
pub fn find_appimages(dirs: &[PathBuf]) -> Vec<(PathBuf, u64, Option<i64>)> {
    use std::os::unix::fs::MetadataExt;
    let mut found = Vec::new();
    for dir in dirs {
        let Ok(read) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if !name.ends_with(".appimage") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_file() {
                found.push((e.path(), meta.blocks() * 512, Some(meta.mtime())));
            }
        }
    }
    found.sort();
    found
}

/// "myapp-1.2.3-linux-x86_64.AppImage" → ("myapp", "1.2.3").
pub fn appimage_name_version(file_name: &str) -> (String, String) {
    let stem = file_name
        .strip_suffix(".AppImage")
        .or_else(|| file_name.strip_suffix(".appimage"))
        .unwrap_or(file_name);
    let parts: Vec<&str> = stem.split(['-', '_']).collect();
    let version_at = parts.iter().position(|p| {
        p.trim_start_matches('v')
            .starts_with(|c: char| c.is_ascii_digit())
    });
    match version_at {
        Some(i) if i > 0 => (
            parts[..i].join("-"),
            parts[i].trim_start_matches('v').to_owned(),
        ),
        _ => (stem.to_owned(), String::new()),
    }
}

/// Why a package may not be removed.
pub const PROTECTED_SYSTEM: &str = "Your system needs it to start and work";
pub const PROTECTED_KERNEL: &str = "It's the kernel you're running right now";

/// Packages from the system package manager, with names and icons from their
/// launchers.
fn native_packages(backend: Backend, launchers: &HashMap<String, DesktopEntry>) -> Vec<Package> {
    let system_launchers: Vec<PathBuf> =
        ["/usr/share/applications", "/usr/local/share/applications"]
            .iter()
            .flat_map(|d| desktop::read_dir_entries(Path::new(d)))
            .map(|e| e.path)
            .collect();
    let owners = backend.owners_of(&system_launchers);
    // Package name → its most fitting visible launcher.
    let mut by_package: HashMap<&str, &DesktopEntry> = HashMap::new();
    for (path, pkg) in &owners {
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(entry) = launchers.get(id).filter(|e| !e.hidden && e.is_application) else {
            continue;
        };
        by_package
            .entry(pkg.as_str())
            .and_modify(|best| {
                if launcher_rank(pkg, entry) < launcher_rank(pkg, best) {
                    *best = entry;
                }
            })
            .or_insert(entry);
    }

    let protected = backend.protected();
    let kernel = backend.kernel_package();
    backend
        .list()
        .into_iter()
        .map(|e| {
            let launcher = by_package.get(e.name.as_str());
            let reason = if kernel.as_deref() == Some(e.name.as_str()) {
                Some(PROTECTED_KERNEL)
            } else if protected.contains(&e.name) {
                Some(PROTECTED_SYSTEM)
            } else {
                None
            };
            Package {
                display_name: launcher
                    .map(|l| l.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| e.name.clone()),
                icon: launcher.and_then(|l| l.icon.clone()),
                is_app: launcher.is_some(),
                name: e.name,
                version: e.version,
                description: e.description,
                source: Source::Native(backend),
                size: e.size,
                installed: e.install_date,
                explicit: e.explicit,
                path: None,
                installation: None,
                protected: reason,
            }
        })
        .collect()
}

/// Installed snaps (current revisions only).
fn snap_packages() -> Vec<Package> {
    let all = crate::snap::list_all();
    if all.is_empty() {
        return Vec::new();
    }
    let launchers = crate::snap::launchers();
    all.into_iter()
        .filter(|s| !s.disabled())
        .map(|s| {
            let launcher = launchers.get(&s.name);
            let size = s.size();
            Package {
                display_name: launcher
                    .map(|l| l.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| s.name.clone()),
                icon: launcher.and_then(|l| l.icon.clone()),
                is_app: launcher.is_some() && !s.is_system(),
                description: launcher.map(|l| l.comment.clone()).unwrap_or_default(),
                installed: std::fs::metadata(s.file())
                    .ok()
                    .map(|m| std::os::unix::fs::MetadataExt::mtime(&m)),
                protected: s.is_system().then_some(PROTECTED_SYSTEM),
                name: s.name,
                version: s.version,
                source: Source::Snap,
                size,
                explicit: true,
                path: None,
                installation: None,
            }
        })
        .collect()
}

/// Everything installed, from every source found on this system. Slow-ish
/// (reads the whole package database), so call it off the main thread.
pub fn list_installed() -> Vec<Package> {
    let launchers = desktop::read_all();
    let mut packages = Vec::new();

    if let Some(backend) = Backend::detect() {
        packages.extend(native_packages(backend, &launchers));
    }
    packages.extend(snap_packages());

    for f in flatpak_list(false) {
        let launcher = launchers.get(&f.id);
        packages.push(Package {
            display_name: if f.name.is_empty() {
                f.id.clone()
            } else {
                f.name.clone()
            },
            icon: launcher
                .and_then(|l| l.icon.clone())
                .or_else(|| Some(f.id.clone())),
            is_app: true,
            description: launcher.map(|l| l.comment.clone()).unwrap_or_default(),
            name: f.id,
            version: f.version,
            source: Source::Flatpak,
            size: f.size,
            installed: None,
            explicit: true,
            path: None,
            installation: Some(f.installation),
            protected: None,
        });
    }

    if let Some(home) = dirs::home_dir() {
        for (path, size, mtime) in find_appimages(&appimage_dirs(&home)) {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (name, version) = appimage_name_version(&file_name);
            // A launcher pointing at this file gives a nicer name and icon.
            let launcher = launchers.values().find(|l| {
                l.exec
                    .as_deref()
                    .is_some_and(|x| x.contains(&*path.to_string_lossy()))
            });
            packages.push(Package {
                display_name: launcher
                    .map(|l| l.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(name),
                icon: launcher.and_then(|l| l.icon.clone()),
                is_app: true,
                description: path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                name: file_name,
                version,
                source: Source::AppImage,
                size,
                installed: mtime,
                explicit: true,
                path: Some(path),
                installation: None,
                protected: None,
            });
        }
    }

    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC: &str = "\
%NAME%
firefox

%VERSION%
159.0.1-1.1

%DESC%
Fast, Private & Safe Web Browser

%INSTALLDATE%
1793914745

%SIZE%
316540722

%REASON%
1

%DEPENDS%
gtk3
libfoo.so=3-64
nss>=3.90

%OPTDEPENDS%
libnotify: desktop notifications
speech-dispatcher>=0.9: text to speech

%PROVIDES%
web-browser=1.0
";

    #[test]
    fn parses_desc() {
        let e = parse_pacman_desc(DESC);
        assert_eq!(e.name, "firefox");
        assert_eq!(e.version, "159.0.1-1.1");
        assert_eq!(e.description, "Fast, Private & Safe Web Browser");
        assert_eq!(e.size, 316_540_722);
        assert_eq!(e.install_date, Some(1_793_914_745));
        assert!(e.as_dependency);
        assert_eq!(e.depends, ["gtk3", "libfoo.so=3-64", "nss>=3.90"]);
        assert_eq!(e.optdepends.len(), 2);
        assert_eq!(e.provides, ["web-browser=1.0"]);
    }

    fn pkg(name: &str, dep: bool, depends: &[&str], provides: &[&str]) -> PacmanEntry {
        PacmanEntry {
            name: name.into(),
            as_dependency: dep,
            depends: depends
                .iter()
                .filter(|s| !s.contains(':'))
                .map(|s| (*s).into())
                .collect(),
            optdepends: depends
                .iter()
                .filter(|s| s.contains(':'))
                .map(|s| (*s).into())
                .collect(),
            provides: provides.iter().map(|s| (*s).into()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn finds_orphans_like_pacman() {
        let db = vec![
            pkg(
                "app",
                false,
                &["libneeded>=2", "sh", "liboptional: extra feature"],
                &[],
            ),
            pkg("liboptional", true, &[], &[]),
            pkg("libneeded", true, &[], &[]),
            pkg("bash", true, &[], &["sh=5.2"]),
            pkg("liborphan", true, &[], &[]),
            pkg("explicit-unused", false, &[], &[]),
        ];
        let names: Vec<&str> = pacman_orphans(&db)
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["liborphan"]);
    }

    #[test]
    fn matches_pacman_on_this_system() {
        let db = Path::new(PACMAN_DB);
        if !db.is_dir() {
            return;
        }
        let Ok(out) = crate::host::command("pacman").arg("-Qdtq").output() else {
            return;
        };
        let mut expected: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        expected.sort();
        let entries = read_pacman_db(db);
        let mut ours: Vec<String> = pacman_orphans(&entries)
            .iter()
            .map(|e| e.name.clone())
            .collect();
        ours.sort();
        assert_eq!(ours, expected);
    }

    #[test]
    fn picks_the_main_launcher() {
        let launcher = |id: &str, name: &str| DesktopEntry {
            id: id.into(),
            name: name.into(),
            ..Default::default()
        };
        let base = launcher("libreoffice-base", "LibreOffice Base");
        let start = launcher("libreoffice-startcenter", "LibreOffice");
        assert!(
            launcher_rank("libreoffice-fresh", &start) < launcher_rank("libreoffice-fresh", &base)
        );
        let resolve = launcher("com.blackmagicdesign.resolve", "DaVinci Resolve Studio");
        let panels = launcher("DaVinciControlPanelsSetup", "DaVinci Control Panels Setup");
        assert!(
            launcher_rank("davinci-resolve-studio", &resolve)
                < launcher_rank("davinci-resolve-studio", &panels)
        );
    }

    #[test]
    fn human_sizes() {
        assert_eq!(parse_human_size("12.3 MB"), Some(12_300_000));
        assert_eq!(parse_human_size("1.0\u{a0}GB"), Some(1_000_000_000));
        assert_eq!(parse_human_size("512 bytes"), Some(512));
        assert_eq!(parse_human_size("2 KiB"), Some(2048));
        assert_eq!(parse_human_size("lots"), None);
    }

    #[test]
    fn parses_flatpak_list() {
        let text = "org.gnome.Maps\tMaps\t47.1\tstable\tx86_64\tsystem\t12.3\u{a0}MB\torg.gnome.Platform/x86_64/47\n\
                    broken line\n";
        let refs = parse_flatpak_list(text);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "org.gnome.Maps");
        assert_eq!(refs[0].size, 12_300_000);
        assert_eq!(refs[0].runtime, "org.gnome.Platform/x86_64/47");
    }

    #[test]
    fn appimage_names() {
        assert_eq!(
            appimage_name_version("myapp-1.2.3-linux-x86_64.AppImage"),
            ("myapp".to_owned(), "1.2.3".to_owned())
        );
        assert_eq!(
            appimage_name_version("Obsidian-v1.8.4.AppImage"),
            ("Obsidian".to_owned(), "1.8.4".to_owned())
        );
        assert_eq!(
            appimage_name_version("tool.AppImage"),
            ("tool".to_owned(), String::new())
        );
    }
}
