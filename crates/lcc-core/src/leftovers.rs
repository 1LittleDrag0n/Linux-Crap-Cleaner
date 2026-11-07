//! Leftovers: things left behind by updates and uninstalled apps.
//!
//! * orphan packages nothing depends on (pacman)
//! * Flatpak runtimes no app uses, and data of removed Flatpak apps
//! * settings folders of apps that aren't installed any more
//! * launchers whose program is gone, and broken symlinks
//! * unfinished downloads
//! * `.pacnew` / `.pacsave` files waiting for review
//!
//! Detection of app settings is a guess, so those items are never selected by
//! default, skip anything changed in the last 30 days, and go to the Trash.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::desktop::{self, DesktopEntry};
use crate::junk::{Places, measure};
use crate::packages::{self, FlatpakRef, PacmanEntry};
use crate::safety::PathGuard;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Packages,
    Flatpak,
    AppSettings,
    Launchers,
    BrokenLinks,
    Downloads,
    SystemConfig,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Packages,
        Category::Flatpak,
        Category::AppSettings,
        Category::Launchers,
        Category::BrokenLinks,
        Category::Downloads,
        Category::SystemConfig,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Packages => "Orphan Packages",
            Category::Flatpak => "Flatpak",
            Category::AppSettings => "Settings of Removed Apps",
            Category::Launchers => "Broken App Launchers",
            Category::BrokenLinks => "Broken Links",
            Category::Downloads => "Unfinished Downloads",
            Category::SystemConfig => "Configuration Files to Review",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Category::Packages => {
                "Installed as dependencies of apps that have since been removed. Nothing needs them now."
            }
            Category::Flatpak => {
                "Runtimes no installed app uses, and data of Flatpak apps you removed."
            }
            Category::AppSettings => {
                "Folders that look like they belong to apps that aren't installed. Check before removing: this is a best guess."
            }
            Category::Launchers => "Menu entries for programs that no longer exist.",
            Category::BrokenLinks => "Shortcuts pointing to files that no longer exist.",
            Category::Downloads => "Partial downloads more than a day old.",
            Category::SystemConfig => {
                "New default settings from updates that were saved next to your own. Compare and merge them by hand."
            }
        }
    }
}

/// What can be done with a leftover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A file or folder in your home: move to Trash or delete.
    Remove,
    /// An orphan package, removed with pacman as administrator.
    RemoveOrphanPackage,
    /// Uninstall a Flatpak ref.
    FlatpakUninstall {
        reference: String,
        installation: String,
    },
    /// Nothing to remove automatically; shown for information.
    Review,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub category: Category,
    pub name: String,
    pub detail: String,
    /// File or folder, when there is one.
    pub path: Option<PathBuf>,
    pub size: u64,
    pub modified: Option<i64>,
    /// Selected when first shown.
    pub default_on: bool,
    pub action: Action,
}

/// Looks for every kind of leftover. Slow-ish; call off the main thread.
pub fn find_leftovers(places: &Places, guard: &PathGuard) -> Vec<Leftover> {
    let db = Path::new(packages::PACMAN_DB);
    let pacman = if db.is_dir() {
        packages::read_pacman_db(db)
    } else {
        Vec::new()
    };
    let flatpak_apps = packages::flatpak_list(false);
    let flatpak_runtimes = packages::flatpak_list(true);
    let launchers = desktop::read_all();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let now = now();

    let mut found = Vec::new();
    found.extend(orphan_leftovers(&pacman));
    found.extend(unused_runtimes(&flatpak_apps, &flatpak_runtimes));
    let app_ids: HashSet<String> = flatpak_apps.iter().map(|f| f.id.clone()).collect();
    found.extend(removed_flatpak_data(
        &places.home.join(".var/app"),
        &app_ids,
    ));

    let known = known_app_names(&pacman, &flatpak_apps, launchers.values(), &path_var);
    for dir in [&places.config, &places.data] {
        found.extend(app_settings_leftovers(dir, &known, now));
    }

    found.extend(broken_launchers(
        &places.data.join("applications"),
        &path_var,
    ));
    let mut link_roots = vec![
        (places.config.clone(), 3),
        (places.data.clone(), 3),
        (places.home.join(".local/bin"), 1),
        (places.home.clone(), 1),
    ];
    link_roots.dedup();
    for (root, depth) in &link_roots {
        found.extend(broken_symlinks(root, *depth, places));
    }
    for dir in packages::appimage_dirs(&places.home) {
        found.extend(unfinished_downloads(&dir, now));
    }
    if db.is_dir() {
        found.extend(pacnew_files(Path::new("/etc")));
    }

    // Everything removable must pass the safety rules.
    found.retain(|l| match (&l.action, &l.path) {
        (Action::Remove, Some(p)) => guard.check(p).is_ok(),
        _ => true,
    });
    found.sort_by(|a, b| a.category.cmp(&b.category).then(b.size.cmp(&a.size)));
    found
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn orphan_leftovers(db: &[PacmanEntry]) -> Vec<Leftover> {
    packages::pacman_orphans(db)
        .into_iter()
        .map(|e| Leftover {
            category: Category::Packages,
            name: e.name.clone(),
            detail: format!("{} · {}", e.version, e.description),
            path: None,
            size: e.size,
            modified: e.install_date,
            default_on: true,
            action: Action::RemoveOrphanPackage,
        })
        .collect()
}

/// Base runtimes (ids ending in `.Platform` or `.Sdk`) that no installed app
/// uses. Extensions such as GL drivers, codecs and translations are left
/// alone: Flatpak manages those with the runtime they extend.
pub fn unused_runtimes(apps: &[FlatpakRef], runtimes: &[FlatpakRef]) -> Vec<Leftover> {
    let used: HashSet<&str> = apps.iter().map(|a| a.runtime.as_str()).collect();
    runtimes
        .iter()
        .filter(|r| r.id.ends_with(".Platform") || r.id.ends_with(".Sdk"))
        // Apps only need their Platform to run; an Sdk is only for building
        // apps, so it counts as unused unless an app names it as its runtime.
        .filter(|r| !used.contains(format!("{}/{}/{}", r.id, r.arch, r.branch).as_str()))
        .map(|r| Leftover {
            category: Category::Flatpak,
            name: format!("{} {}", r.id, r.branch),
            detail: format!(
                "Runtime no installed app uses · {} installation",
                r.installation
            ),
            path: None,
            size: r.size,
            modified: None,
            default_on: true,
            action: Action::FlatpakUninstall {
                reference: format!("runtime/{}/{}/{}", r.id, r.arch, r.branch),
                installation: r.installation.clone(),
            },
        })
        .collect()
}

/// `~/.var/app/<id>` folders of Flatpak apps that aren't installed.
pub fn removed_flatpak_data(var_app: &Path, installed: &HashSet<String>) -> Vec<Leftover> {
    let Ok(read) = std::fs::read_dir(var_app) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !installed.contains(&*e.file_name().to_string_lossy()))
        .map(|e| {
            let path = e.path();
            let (size, _) = measure(&path);
            Leftover {
                category: Category::Flatpak,
                name: e.file_name().to_string_lossy().into_owned(),
                detail: "Data of a removed Flatpak app (settings, saves, cache)".to_owned(),
                modified: newest_mtime(&path),
                path: Some(path),
                size,
                default_on: false,
                action: Action::Remove,
            }
        })
        .collect()
}

/// Lowercase letters and digits only: "Brave-Software" → "bravesoftware".
pub fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Parts of package names too generic to identify an app.
const GENERIC: &[&str] = &[
    "bin", "git", "lib", "libs", "python", "python3", "perl", "ruby", "plugin", "plugins", "theme",
    "themes", "data", "common", "utils", "tools", "desktop", "electron", "fonts", "font",
    "runtime", "base", "core", "daemon", "server", "client", "icon", "icons", "devel", "docs",
    "extra", "full", "nightly", "beta", "stable", "linux", "gnome", "kde", "qt5", "qt6", "gtk2",
    "gtk3", "gtk4", "x11", "wayland", "the",
];

/// Folders that belong to the desktop or to many apps, never flagged.
const NEVER_FLAG: &[&str] = &[
    "autostart",
    "systemd",
    "environment.d",
    "fontconfig",
    "fonts",
    "gtk-2.0",
    "gtk-3.0",
    "gtk-4.0",
    "dconf",
    "pulse",
    "pipewire",
    "wireplumber",
    "menus",
    "mime",
    "applications",
    "icons",
    "themes",
    "color-schemes",
    "xdg-desktop-portal",
    "Trash",
    "keyrings",
    "flatpak",
    "sounds",
    "desktop-directories",
    "gvfs-metadata",
    "recently-used.xbel",
    "session",
    "sessions",
    "xorg",
    "locale",
    "backgrounds",
    "wallpapers",
    "color",
    "Electron",
    "QtProject",
    "nvim",
    "vim",
    "user-places.xbel",
    "kxmlgui5",
    "kxmlgui6",
    "kded5",
    "kded6",
    "kwalletd",
    "kdedefaults",
    "baloo",
    "akonadi",
    "plasma-workspace",
    "kactivitymanagerd",
    "kscreen",
    "knewstuff3",
    "dbus-1",
    "evolution",
    "tracker",
    "tracker3",
    "gnome-shell",
    "nautilus",
    "ibus",
    "fcitx",
    "fcitx5",
    "mozilla",
    "Mousepad",
    "xfce4",
    "Thunar",
    "lxsession",
    "openbox",
    "labwc",
    "hypr",
    "niri",
    "sway",
    "waybar",
    "mako",
    "dunst",
    "rofi",
    "wofi",
    "fuzzel",
    "kitty",
    "alacritty",
    "foot",
    "wezterm",
    "ghostty",
    "starship.toml",
    "fish",
    "zsh",
    "bash",
    "git",
    "gh",
    "htop",
    "btop",
    "go",
    "pip",
    "uv",
    "cargo",
    "rustup",
    "npm",
    "yarn",
    "pnpm",
    "docker",
    "containers",
    "lcc",
    "linux-crap-cleaner",
    "user-dirs.dirs",
    "user-dirs.locale",
    "mimeapps.list",
    "pavucontrol.ini",
    "qt5ct",
    "qt6ct",
    "Kvantum",
    "nwg-look",
];

/// Names that identify installed software: package names and their
/// distinctive parts, Flatpak ids, launcher names and programs in `PATH`.
pub fn known_app_names<'a>(
    pacman: &[PacmanEntry],
    flatpak_apps: &[FlatpakRef],
    launchers: impl Iterator<Item = &'a DesktopEntry>,
    path_var: &std::ffi::OsStr,
) -> HashSet<String> {
    let mut known = HashSet::new();
    let mut add = |s: &str| {
        let n = normalize(s);
        if n.len() >= 3 {
            known.insert(n);
        }
    };
    for p in pacman {
        add(&p.name);
        for part in p.name.split(['-', '_', '.']) {
            if part.len() >= 4 && !GENERIC.contains(&part) {
                add(part);
            }
        }
    }
    for f in flatpak_apps {
        add(&f.id);
        add(&f.name);
        if let Some(last) = f.id.rsplit('.').next() {
            add(last);
        }
    }
    for l in launchers {
        add(&l.id);
        add(&l.name);
        if let Some(last) = l.id.rsplit('.').next() {
            add(last);
        }
        if let Some(program) = desktop::launcher_program(l)
            && let Some(base) = Path::new(&program).file_name()
        {
            add(&base.to_string_lossy());
        }
    }
    for dir in std::env::split_paths(path_var) {
        if let Ok(read) = std::fs::read_dir(dir) {
            for e in read.flatten() {
                add(&e.file_name().to_string_lossy());
            }
        }
    }
    known
}

/// Whether a folder name looks like it belongs to something known.
pub fn matches_known(folder: &str, known: &HashSet<String>) -> bool {
    let f = normalize(folder);
    if f.len() < 3 || known.contains(&f) {
        return true;
    }
    known.iter().any(|k| {
        (k.len() >= 4 && f.contains(k.as_str())) || (f.len() >= 4 && k.contains(f.as_str()))
    })
}

const THIRTY_DAYS: i64 = 30 * 24 * 3600;

/// Folders directly inside `dir` that don't match any installed software and
/// haven't changed in 30 days.
pub fn app_settings_leftovers(dir: &Path, known: &HashSet<String>, now: i64) -> Vec<Leftover> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if NEVER_FLAG.contains(&name.as_str()) || matches_known(&name, known) {
                return None;
            }
            let path = e.path();
            let modified = newest_mtime(&path);
            if modified.is_some_and(|m| now - m < THIRTY_DAYS) {
                return None;
            }
            let (size, files) = measure(&path);
            Some(Leftover {
                category: Category::AppSettings,
                detail: format!("{} · {} files", path.display(), files),
                name,
                path: Some(path),
                size,
                modified,
                default_on: false,
                action: Action::Remove,
            })
        })
        .collect()
}

/// Newest modification time of a folder and the entries directly in it.
fn newest_mtime(path: &Path) -> Option<i64> {
    let own = std::fs::symlink_metadata(path).ok()?.mtime();
    let children = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok().map(|m| m.mtime()));
    Some(children.fold(own, i64::max))
}

/// Launchers in `dir` whose program no longer exists.
pub fn broken_launchers(dir: &Path, path_var: &std::ffi::OsStr) -> Vec<Leftover> {
    desktop::read_dir_entries(dir)
        .into_iter()
        .filter(|e| desktop::is_broken(e, path_var))
        .map(|e| {
            let size = std::fs::symlink_metadata(&e.path)
                .map(|m| m.blocks() * 512)
                .unwrap_or(0);
            let program = desktop::launcher_program(&e).unwrap_or_default();
            Leftover {
                category: Category::Launchers,
                name: if e.name.is_empty() {
                    e.id.clone()
                } else {
                    e.name.clone()
                },
                detail: format!("Runs {program}, which no longer exists"),
                modified: std::fs::symlink_metadata(&e.path).ok().map(|m| m.mtime()),
                path: Some(e.path),
                size,
                default_on: true,
                action: Action::Remove,
            }
        })
        .collect()
}

/// Symlinks under `root` (up to `depth` levels) whose target is missing.
/// Never enters symlinked folders, the Trash, or the cache folder.
pub fn broken_symlinks(root: &Path, depth: usize, places: &Places) -> Vec<Leftover> {
    let skip = [
        places.data.join("Trash"),
        places.cache.clone(),
        places.home.join(".var"),
    ];
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, level)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in read.flatten() {
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                let target = std::fs::read_link(&path).unwrap_or_default();
                if std::fs::metadata(&path).is_err()
                    && is_real_broken_link(&e.file_name().to_string_lossy(), &target)
                {
                    found.push(Leftover {
                        category: Category::BrokenLinks,
                        name: e.file_name().to_string_lossy().into_owned(),
                        detail: format!("{} → {} (missing)", path.display(), target.display()),
                        modified: std::fs::symlink_metadata(&path).ok().map(|m| m.mtime()),
                        path: Some(path),
                        size: 0,
                        default_on: true,
                        action: Action::Remove,
                    });
                }
            } else if ft.is_dir() && level + 1 < depth && !skip.contains(&path) {
                stack.push((path, level + 1));
            }
        }
    }
    found
}

/// Apps such as Electron programs and Firefox use dangling symlinks on
/// purpose as "already running" locks (`SingletonLock` → `host-1234`). Those
/// must never be touched. A link counts as broken only if it points at a
/// file path and isn't a known lock.
pub fn is_real_broken_link(name: &str, target: &Path) -> bool {
    let lock_name = name.starts_with("Singleton") || name == "lock" || name == ".parentlock";
    !lock_name && target.to_string_lossy().contains('/')
}

const DOWNLOAD_SUFFIXES: &[&str] = &[".part", ".crdownload", ".download", ".tmp", ".partial"];
const ONE_DAY: i64 = 24 * 3600;

/// Partial downloads in `dir` older than a day.
pub fn unfinished_downloads(dir: &Path, now: i64) -> Vec<Leftover> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            if !DOWNLOAD_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() || now - meta.mtime() < ONE_DAY {
                return None;
            }
            Some(Leftover {
                category: Category::Downloads,
                detail: e.path().display().to_string(),
                name,
                path: Some(e.path()),
                size: meta.blocks() * 512,
                modified: Some(meta.mtime()),
                default_on: true,
                action: Action::Remove,
            })
        })
        .collect()
}

/// `.pacnew` and `.pacsave` files under `root` (normally `/etc`).
pub fn pacnew_files(root: &Path) -> Vec<Leftover> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in read.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let path = e.path();
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = if name.ends_with(".pacnew") {
                "New default settings from an update"
            } else if name.ends_with(".pacsave") {
                "Your old settings, saved when a package was removed"
            } else {
                continue;
            };
            let meta = std::fs::symlink_metadata(&path).ok();
            found.push(Leftover {
                category: Category::SystemConfig,
                name,
                detail: format!("{kind} · {}", path.display()),
                size: meta.as_ref().map(|m| m.blocks() * 512).unwrap_or(0),
                modified: meta.map(|m| m.mtime()),
                path: Some(path),
                default_on: false,
                action: Action::Review,
            });
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Uninstalls a Flatpak ref. For the system installation, Flatpak asks for
/// the administrator password itself.
pub fn flatpak_uninstall(reference: &str, installation: &str) -> Result<(), String> {
    let scope = if installation == "user" {
        "--user"
    } else {
        "--system"
    };
    let output = Command::new("flatpak")
        .args(["uninstall", "--noninteractive", "-y", scope, reference])
        .output()
        .map_err(|e| format!("couldn't run flatpak: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lcc-left-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn places(home: &Path) -> Places {
        Places {
            home: home.to_path_buf(),
            cache: home.join(".cache"),
            config: home.join(".config"),
            data: home.join(".local/share"),
            state: home.join(".local/state"),
        }
    }

    fn flatpak(id: &str, branch: &str, runtime: &str) -> FlatpakRef {
        FlatpakRef {
            id: id.into(),
            name: id.into(),
            version: String::new(),
            branch: branch.into(),
            arch: "x86_64".into(),
            installation: "system".into(),
            size: 1000,
            runtime: runtime.into(),
        }
    }

    #[test]
    fn unused_runtimes_skip_used_and_extensions() {
        let apps = vec![flatpak(
            "org.gnome.Maps",
            "stable",
            "org.gnome.Platform/x86_64/47",
        )];
        let runtimes = vec![
            flatpak("org.gnome.Platform", "47", ""),
            flatpak("org.gnome.Platform", "45", ""),
            flatpak("org.freedesktop.Platform.GL.default", "24.08", ""),
            flatpak("org.kde.Sdk", "6.8", ""),
        ];
        let unused = unused_runtimes(&apps, &runtimes);
        let names: Vec<&str> = unused.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["org.gnome.Platform 45", "org.kde.Sdk 6.8"]);
        assert_eq!(
            unused[0].action,
            Action::FlatpakUninstall {
                reference: "runtime/org.gnome.Platform/x86_64/45".into(),
                installation: "system".into()
            }
        );
    }

    #[test]
    fn removed_flatpak_app_data() {
        let t = Temp::new("var");
        t.file("org.gnome.Maps/config/x", "1");
        t.file("com.removed.App/data/y", "2");
        let installed: HashSet<String> = ["org.gnome.Maps".to_owned()].into();
        let found = removed_flatpak_data(&t.0, &installed);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "com.removed.App");
        assert!(!found[0].default_on);
    }

    #[test]
    fn known_name_matching() {
        let known: HashSet<String> = ["brave", "code", "spotify", "googlechrome"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(matches_known("BraveSoftware", &known));
        assert!(matches_known("Code", &known));
        assert!(matches_known("google-chrome", &known));
        assert!(matches_known("ab", &known), "too short to judge: keep");
        assert!(!matches_known("discord", &known));
        assert!(!matches_known("Slack", &known));
    }

    #[test]
    fn known_names_from_packages() {
        let pacman = vec![PacmanEntry {
            name: "intellij-idea-community-edition".into(),
            ..Default::default()
        }];
        let known = known_app_names(&pacman, &[], std::iter::empty(), std::ffi::OsStr::new(""));
        assert!(known.contains("intellij"));
        assert!(known.contains("community"));
        assert!(!known.contains("bin"));
    }

    #[test]
    fn settings_of_removed_apps() {
        let t = Temp::new("settings");
        t.file("discord/settings.json", "{}");
        t.file("Code/settings.json", "{}");
        t.file("autostart/x.desktop", "");
        let known: HashSet<String> = ["code".to_owned()].into();

        // Everything was just written, so nothing is old enough yet.
        assert!(app_settings_leftovers(&t.0, &known, now()).is_empty());

        let later = now() + 31 * 24 * 3600;
        let found = app_settings_leftovers(&t.0, &known, later);
        let names: Vec<&str> = found.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["discord"]);
        assert!(!found[0].default_on);
    }

    #[test]
    fn broken_launchers_and_links() {
        let t = Temp::new("broken");
        let apps = t.0.join(".local/share/applications");
        t.file(
            ".local/share/applications/gone.desktop",
            "[Desktop Entry]\nType=Application\nName=Gone App\nExec=/nonexistent/Gone.AppImage %U\n",
        );
        t.file(
            ".local/share/applications/ok.desktop",
            "[Desktop Entry]\nType=Application\nName=Shell\nExec=sh\n",
        );
        let path = std::ffi::OsString::from("/usr/bin:/bin");
        let found = broken_launchers(&apps, &path);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Gone App");

        fs::create_dir_all(t.0.join(".config/app")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", t.0.join(".config/app/dangling"))
            .unwrap();
        std::os::unix::fs::symlink("/usr", t.0.join(".config/fine")).unwrap();
        fs::create_dir_all(t.0.join(".cache")).unwrap();
        std::os::unix::fs::symlink("/nonexistent", t.0.join(".cache/ignored")).unwrap();
        let p = places(&t.0);
        let links = broken_symlinks(&p.config, 3, &p);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].name, "dangling");
        assert!(
            broken_symlinks(&p.home, 1, &p).is_empty(),
            "depth 1 doesn't reach .config/app"
        );

        // App locks are dangling on purpose.
        std::os::unix::fs::symlink("myhost-1234", t.0.join(".config/app/SingletonLock")).unwrap();
        std::os::unix::fs::symlink("127.0.1.1:+99", t.0.join(".config/app/lock")).unwrap();
        assert_eq!(broken_symlinks(&p.config, 3, &p).len(), 1);
        assert!(!is_real_broken_link("cache", Path::new("host-42")));
        assert!(is_real_broken_link(
            ".steampath",
            Path::new("/home/x/.steam/steam")
        ));
    }

    #[test]
    fn partial_downloads_older_than_a_day() {
        let t = Temp::new("downloads");
        t.file("movie.mkv.part", "x");
        t.file("app.AppImage", "x");
        assert!(unfinished_downloads(&t.0, now()).is_empty());
        let found = unfinished_downloads(&t.0, now() + 2 * ONE_DAY);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "movie.mkv.part");
    }

    #[test]
    fn finds_pacnew_files() {
        let t = Temp::new("etc");
        t.file("pacman.conf.pacnew", "x");
        t.file("ssh/sshd_config.pacsave", "x");
        t.file("fstab", "x");
        let found = pacnew_files(&t.0);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|l| l.action == Action::Review));
    }
}
