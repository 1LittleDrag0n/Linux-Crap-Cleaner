//! App usage: when each installed app was last used.
//!
//! Linux doesn't record this, and most systems mount disks with `noatime`, so
//! file access times can't tell either. Two sources are combined:
//!
//! * **Estimate** (always): apps write to their own folders in `~/.config`,
//!   `~/.cache`, `~/.local/share` and `~/.var/app` while they run, so the
//!   newest change there approximates the last use.
//! * **Tracking** (opt-in): a small background service ([`run_tracker`])
//!   notes once a minute which of the user's programs are running. Data stays
//!   in `~/.local/share/linux-crap-cleaner/usage.toml`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::junk::Places;
use crate::leftovers::normalize;
use crate::packages::{Package, Source};

/// What the tracker knows about one program.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// Unix time it was last seen running.
    pub last: i64,
    /// Minutes it was seen running.
    pub minutes: u64,
}

/// The tracker's records, keyed by program (see [`process_keys`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageDb {
    /// Unix time tracking started.
    #[serde(default)]
    pub since: i64,
    #[serde(default)]
    pub seen: BTreeMap<String, Seen>,
}

impl UsageDb {
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join(crate::APP_DIR_NAME).join("usage.toml"))
    }

    /// Loads the records; missing or unreadable data gives an empty database.
    pub fn load() -> Self {
        Self::default_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }

    /// Records that `keys` were running at `now`, for `minutes` minutes.
    pub fn record(&mut self, keys: &HashSet<String>, now: i64, minutes: u64) {
        if self.since == 0 {
            self.since = now;
        }
        for k in keys {
            let s = self.seen.entry(k.clone()).or_default();
            s.last = now;
            s.minutes += minutes;
        }
    }
}

/// Keys for one running process: `flatpak:<id>`, `appimage:<path>`,
/// `exe:<path>`, and `arg:<path>` for scripts and Electron apps whose real
/// program is a file given on the command line.
pub fn process_keys(environ: &[u8], exe: Option<&Path>, cmdline: &[u8]) -> Vec<String> {
    let mut keys = Vec::new();
    for var in environ.split(|&b| b == 0) {
        if let Some(id) = var.strip_prefix(b"FLATPAK_ID=") {
            keys.push(format!("flatpak:{}", String::from_utf8_lossy(id)));
        } else if let Some(path) = var.strip_prefix(b"APPIMAGE=") {
            keys.push(format!("appimage:{}", String::from_utf8_lossy(path)));
        }
    }
    if let Some(exe) = exe {
        keys.push(format!("exe:{}", exe.display()));
    }
    for arg in cmdline.split(|&b| b == 0).skip(1).take(3) {
        if arg.first() == Some(&b'/') {
            let p = Path::new(std::ffi::OsStr::from_bytes(arg));
            if p.is_file() {
                keys.push(format!("arg:{}", p.display()));
            }
        }
    }
    keys
}

/// Keys of every running process that belongs to the current user.
pub fn running_keys() -> HashSet<String> {
    let uid = rustix::process::getuid().as_raw();
    let mut keys = HashSet::new();
    let Ok(read) = std::fs::read_dir("/proc") else {
        return keys;
    };
    for e in read.flatten() {
        if !e.file_name().as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let dir = e.path();
        if std::fs::metadata(&dir).map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let environ = std::fs::read(dir.join("environ")).unwrap_or_default();
        let cmdline = std::fs::read(dir.join("cmdline")).unwrap_or_default();
        let exe = std::fs::read_link(dir.join("exe")).ok();
        keys.extend(process_keys(&environ, exe.as_deref(), &cmdline));
    }
    keys
}

/// Runs the tracker: samples running programs every `interval` and saves the
/// records. With `once`, takes a single sample and returns.
pub fn run_tracker(interval: std::time::Duration, once: bool) -> std::io::Result<()> {
    let path = UsageDb::default_path().ok_or_else(|| std::io::Error::other("no data folder"))?;
    let minutes = (interval.as_secs() / 60).max(1);
    loop {
        let mut db = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        let keys = running_keys();
        UsageDb::record(&mut db, &keys, crate::format::now(), minutes);
        db.save_to(&path)?;
        if once {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

const SERVICE: &str = "linux-crap-cleaner-usage.service";

fn service_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("systemd/user").join(SERVICE))
}

/// The systemd user service that runs `program --track-usage`.
pub fn service_unit(program: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Linux Crap Cleaner app usage tracker (data stays on this computer)\n\
         \n\
         [Service]\n\
         ExecStart=\"{}\" --track-usage\n\
         Restart=on-failure\n\
         RestartSec=60\n\
         Nice=19\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        program.display()
    )
}

pub fn tracker_installed() -> bool {
    service_path().is_some_and(|p| p.exists())
}

/// Turns the background tracker on (install and start the user service) or
/// off (stop and remove it).
pub fn set_tracker_enabled(enabled: bool) -> Result<(), String> {
    let path = service_path().ok_or("no config folder")?;
    let systemctl = |args: &[&str]| -> Result<(), String> {
        let out = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| format!("couldn't run systemctl: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    };
    if enabled {
        let program = std::env::current_exe().map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, service_unit(&program)).map_err(|e| e.to_string())?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", SERVICE])
    } else {
        if path.exists() {
            let _ = systemctl(&["disable", "--now", SERVICE]);
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
            let _ = systemctl(&["daemon-reload"]);
        }
        Ok(())
    }
}

/// How a last-used time was worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Seen running by the tracker.
    Tracked,
    /// From the newest change in the app's own folders.
    Estimated,
    /// Nothing known.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct AppUsage {
    pub package: Package,
    pub last_used: Option<i64>,
    pub basis: Basis,
    /// Minutes seen running (tracking only).
    pub minutes: Option<u64>,
}

/// Name parts too generic to match an app's folder.
const GENERIC: &[&str] = &[
    "bin",
    "git",
    "app",
    "browser",
    "desktop",
    "electron",
    "client",
    "the",
    "gtk",
    "qt",
    "linux",
    "beta",
    "nightly",
    "stable",
    "studio",
    "edition",
    "community",
    "free",
    "open",
];

/// Normalized names that identify an app's own folders: `strong` names (the
/// full package and display name) and `weak` parts of them, which only count
/// when no other app shares them (many Xfce tools share `xfce4`).
fn app_names(p: &Package) -> (HashSet<String>, HashSet<String>) {
    let ok = |n: &String| n.len() >= 3 && !GENERIC.contains(&n.as_str());
    let mut strong: HashSet<String> = [normalize(&p.name), normalize(&p.display_name)]
        .into_iter()
        .filter(ok)
        .collect();
    if p.source == Source::Flatpak
        && let Some(last) = p.name.rsplit('.').next()
    {
        strong.insert(normalize(last));
    }
    let weak: HashSet<String> = p
        .name
        .split(['-', '_', '.'])
        .chain(p.display_name.split([' ', '-']))
        .map(normalize)
        .filter(ok)
        .filter(|n| !strong.contains(n))
        .collect();
    (strong, weak)
}

/// Folders apps keep their data in, by normalized name.
fn app_folders(places: &Places) -> HashMap<String, Vec<PathBuf>> {
    let mut map: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let bases = [
        &places.config,
        &places.cache,
        &places.data,
        &places.home.join(".var/app"),
    ];
    for base in bases {
        let Ok(read) = std::fs::read_dir(base) else {
            continue;
        };
        for e in read.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                let name = e.file_name().to_string_lossy().into_owned();
                map.entry(normalize(&name)).or_default().push(e.path());
                if let Some(last) = name.rsplit('.').next() {
                    map.entry(normalize(last)).or_default().push(e.path());
                }
            }
        }
    }
    map
}

/// Newest change in a folder and the entries directly inside it.
fn newest_change(dir: &Path) -> Option<i64> {
    let own = std::fs::symlink_metadata(dir).ok()?.mtime();
    Some(
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok().map(|m| m.mtime()))
            .fold(own, i64::max),
    )
}

/// Which installed package owns each of `paths` (absolute), from pacman's
/// file lists.
fn pacman_owners(paths: &HashSet<String>) -> HashMap<String, String> {
    let db = Path::new(crate::packages::PACMAN_DB);
    let Ok(read) = std::fs::read_dir(db) else {
        return HashMap::new();
    };
    let mut owners = HashMap::new();
    for e in read.flatten() {
        let Ok(files) = std::fs::read_to_string(e.path().join("files")) else {
            continue;
        };
        let mut owned = Vec::new();
        for line in files.lines() {
            let abs = format!("/{line}");
            if paths.contains(&abs) {
                owned.push(abs);
            }
        }
        if owned.is_empty() {
            continue;
        }
        let name = std::fs::read_to_string(e.path().join("desc"))
            .map(|d| crate::packages::parse_pacman_desc(&d).name)
            .unwrap_or_default();
        for p in owned {
            owners.insert(p, name.clone());
        }
    }
    owners
}

/// Tracked usage per package id (`source:name`).
fn tracked(db: &UsageDb, packages: &[Package]) -> HashMap<String, Seen> {
    let mut by_package: HashMap<String, Seen> = HashMap::new();
    let mut merge = |id: String, s: &Seen| {
        let e = by_package.entry(id).or_default();
        e.last = e.last.max(s.last);
        e.minutes = e.minutes.max(s.minutes);
    };
    let files: HashSet<String> = db
        .seen
        .keys()
        .filter_map(|k| k.strip_prefix("exe:").or_else(|| k.strip_prefix("arg:")))
        .map(str::to_owned)
        .collect();
    let owners = pacman_owners(&files);
    for (key, seen) in &db.seen {
        if let Some(id) = key.strip_prefix("flatpak:") {
            merge(format!("Flatpak:{id}"), seen);
        } else if let Some(path) = key.strip_prefix("appimage:") {
            if let Some(p) = packages
                .iter()
                .find(|p| p.path.as_deref() == Some(Path::new(path)))
            {
                merge(format!("AppImage:{}", p.name), seen);
            }
        } else if let Some(file) = key
            .strip_prefix("exe:")
            .or_else(|| key.strip_prefix("arg:"))
            && let Some(owner) = owners.get(file)
        {
            merge(format!("pacman:{owner}"), seen);
        }
    }
    by_package
}

fn package_id(p: &Package) -> String {
    format!("{}:{}", p.source.name(), p.name)
}

/// Last-use information for every app in `packages` (packages without a
/// launcher are skipped).
pub fn app_usage(packages: Vec<Package>, db: &UsageDb, places: &Places) -> Vec<AppUsage> {
    let folders = app_folders(places);
    let tracked = tracked(db, &packages);
    let apps: Vec<Package> = packages.into_iter().filter(|p| p.is_app).collect();
    let names: Vec<(HashSet<String>, HashSet<String>)> = apps.iter().map(app_names).collect();

    // A weak name shared by several apps (or equal to another app's full
    // name) identifies none of them.
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for (strong, weak) in &names {
        for n in strong.iter().chain(weak) {
            *uses.entry(n.as_str()).or_default() += 1;
        }
    }

    apps.iter()
        .zip(&names)
        .map(|(p, (strong, weak))| {
            let seen = tracked.get(&package_id(p)).copied();
            let estimate = strong
                .iter()
                .chain(weak.iter().filter(|n| uses.get(n.as_str()) == Some(&1)))
                .filter_map(|n| folders.get(n))
                .flatten()
                .filter_map(|d| newest_change(d))
                .max();
            let (last_used, basis) = match (seen, estimate) {
                (Some(s), Some(e)) => (Some(s.last.max(e)), Basis::Tracked),
                (Some(s), None) => (Some(s.last), Basis::Tracked),
                (None, Some(e)) => (Some(e), Basis::Estimated),
                (None, None) => (None, Basis::Unknown),
            };
            AppUsage {
                last_used,
                basis,
                minutes: seen.map(|s| s.minutes),
                package: p.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn app(name: &str, display: &str, source: Source) -> Package {
        Package {
            name: name.into(),
            display_name: display.into(),
            version: String::new(),
            description: String::new(),
            source,
            size: 1,
            installed: None,
            explicit: true,
            is_app: true,
            icon: None,
            path: None,
            installation: None,
            protected: None,
        }
    }

    #[test]
    fn process_keys_cover_flatpak_appimage_and_scripts() {
        let environ = b"HOME=/h\0FLATPAK_ID=org.gnome.Maps\0APPIMAGE=/h/Apps/x.AppImage\0";
        let cmdline = b"/usr/lib/electron/electron\0/bin/sh\0--flag\0";
        let keys = process_keys(
            environ,
            Some(Path::new("/usr/lib/electron/electron")),
            cmdline,
        );
        assert!(keys.contains(&"flatpak:org.gnome.Maps".to_owned()));
        assert!(keys.contains(&"appimage:/h/Apps/x.AppImage".to_owned()));
        assert!(keys.contains(&"exe:/usr/lib/electron/electron".to_owned()));
        assert!(keys.contains(&"arg:/bin/sh".to_owned()), "{keys:?}");
        assert!(!keys.iter().any(|k| k.contains("--flag")));
    }

    #[test]
    fn recording_and_saving() {
        let mut db = UsageDb::default();
        let keys: HashSet<String> = ["exe:/usr/bin/a".to_owned()].into();
        db.record(&keys, 100, 1);
        db.record(&keys, 160, 1);
        assert_eq!(db.since, 100);
        assert_eq!(
            db.seen["exe:/usr/bin/a"],
            Seen {
                last: 160,
                minutes: 2
            }
        );

        let path =
            std::env::temp_dir().join(format!("lcc-usage-{}/usage.toml", std::process::id()));
        db.save_to(&path).unwrap();
        let loaded: UsageDb = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(loaded, db);
    }

    #[test]
    fn names_for_matching_folders() {
        let (strong, weak) = app_names(&app("zen-browser-bin", "Zen Browser", Source::Pacman));
        assert!(strong.contains("zenbrowser"));
        assert!(weak.contains("zen"));
        assert!(!weak.contains("browser") && !weak.contains("bin"));
        let (strong, _) = app_names(&app("org.gnome.Maps", "Maps", Source::Flatpak));
        assert!(strong.contains("maps"));
    }

    #[test]
    fn shared_folders_identify_no_one() {
        let home = std::env::temp_dir().join(format!("lcc-usage-shared-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".config/xfce4")).unwrap();
        let places = Places {
            home: home.clone(),
            cache: home.join(".cache"),
            config: home.join(".config"),
            data: home.join(".local/share"),
            state: home.join(".local/state"),
        };
        let packages = vec![
            app("xfce4-notes-plugin", "Notes", Source::Pacman),
            app("xfce4-dict", "Dictionary", Source::Pacman),
        ];
        let usage = app_usage(packages, &UsageDb::default(), &places);
        fs::remove_dir_all(&home).unwrap();
        assert!(usage.iter().all(|u| u.basis == Basis::Unknown));
    }

    #[test]
    fn estimates_from_app_folders() {
        let home = std::env::temp_dir().join(format!("lcc-usage-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".config/zen")).unwrap();
        fs::create_dir_all(home.join(".cache/spotify")).unwrap();
        let places = Places {
            home: home.clone(),
            cache: home.join(".cache"),
            config: home.join(".config"),
            data: home.join(".local/share"),
            state: home.join(".local/state"),
        };
        let mut db = UsageDb::default();
        db.record(&["flatpak:org.gnome.Maps".to_owned()].into(), 42, 3);
        let packages = vec![
            app("zen-browser-bin", "Zen Browser", Source::Pacman),
            app("spotify", "Spotify", Source::Pacman),
            app("never-used-app", "Never Used", Source::Pacman),
            app("org.gnome.Maps", "Maps", Source::Flatpak),
        ];
        let usage = app_usage(packages, &db, &places);
        fs::remove_dir_all(&home).unwrap();

        let by_name = |n: &str| usage.iter().find(|u| u.package.name == n).unwrap();
        assert_eq!(by_name("zen-browser-bin").basis, Basis::Estimated);
        assert!(by_name("spotify").last_used.is_some());
        assert_eq!(by_name("never-used-app").basis, Basis::Unknown);
        let maps = by_name("org.gnome.Maps");
        assert_eq!(maps.basis, Basis::Tracked);
        assert_eq!(maps.last_used, Some(42));
        assert_eq!(maps.minutes, Some(3));
    }

    #[test]
    fn service_unit_runs_tracker() {
        let unit = service_unit(Path::new("/usr/bin/linux-crap-cleaner"));
        assert!(unit.contains("ExecStart=\"/usr/bin/linux-crap-cleaner\" --track-usage"));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
