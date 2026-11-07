//! Junk Cleaner: finds caches, logs and leftovers described by cleaner rules,
//! measures them, and removes the ones the user picks.
//!
//! Rules live in `cleaners/builtin.toml` (see the comments there for the
//! format). [`analyze`] turns rules into [`Finding`]s with exact sizes, and
//! [`clean`] removes the chosen findings. Nothing is removed by [`analyze`].

use std::collections::{BTreeMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;
use serde::Deserialize;

use crate::privileged::Operation;
use crate::remove::{RemoveError, remove};
use crate::safety::PathGuard;
use crate::scan::{ScanOptions, ScanProgress, scan};
use crate::settings::DeleteMode;

/// Groups shown in the Junk Cleaner, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Browsers,
    Apps,
    Developer,
    System,
    TrashHistory,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::Browsers,
        Category::Apps,
        Category::Developer,
        Category::System,
        Category::TrashHistory,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Browsers => "Web Browsers",
            Category::Apps => "Apps",
            Category::Developer => "Developer Tools",
            Category::System => "System",
            Category::TrashHistory => "Trash & History",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Delete what `paths` matches.
    #[default]
    Paths,
    /// Old versions in the package manager's cache.
    PackageCache,
    /// The systemd journal.
    Journal,
}

/// One cleaner rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cleaner {
    pub id: String,
    pub name: String,
    pub category: Category,
    pub description: String,
    #[serde(default)]
    pub icon: Option<String>,
    /// Checked when first shown.
    #[serde(default, rename = "default")]
    pub default_on: bool,
    /// Process names; while one runs, this cleaner is skipped.
    #[serde(default)]
    pub running: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub needs_root: bool,
    /// Skip folders another cleaner already handles.
    #[serde(default)]
    pub exclude_known: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    cleaner: Vec<Cleaner>,
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("invalid cleaner rules: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("cleaner id `{0}` is used twice")]
    DuplicateId(String),
}

/// Parses a rules file.
pub fn parse_rules(text: &str) -> Result<Vec<Cleaner>, RuleError> {
    let file: RuleFile = toml::from_str(text)?;
    let mut seen = HashSet::new();
    for c in &file.cleaner {
        if !seen.insert(c.id.as_str()) {
            return Err(RuleError::DuplicateId(c.id.clone()));
        }
    }
    Ok(file.cleaner)
}

/// The rules that ship with the app.
pub fn builtin_cleaners() -> Vec<Cleaner> {
    parse_rules(include_str!("../cleaners/builtin.toml")).expect("built-in cleaner rules are valid")
}

/// Standard folders that rule paths refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    pub home: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
}

impl Places {
    /// Places for the current user, following the XDG variables.
    pub fn current() -> Option<Self> {
        let home = dirs::home_dir()?;
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        Some(Self {
            cache: xdg("XDG_CACHE_HOME", ".cache"),
            config: xdg("XDG_CONFIG_HOME", ".config"),
            data: xdg("XDG_DATA_HOME", ".local/share"),
            state: xdg("XDG_STATE_HOME", ".local/state"),
            home,
        })
    }

    /// Replaces `{home}`, `{cache}`, ... in a rule path.
    pub fn expand(&self, pattern: &str) -> String {
        pattern
            .replace("{home}", &self.home.to_string_lossy())
            .replace("{cache}", &self.cache.to_string_lossy())
            .replace("{config}", &self.config.to_string_lossy())
            .replace("{data}", &self.data.to_string_lossy())
            .replace("{state}", &self.state.to_string_lossy())
    }
}

/// Matches one file name against a pattern with `*`, `?` and `[a-z]`.
/// `*` also matches names starting with a dot.
pub fn wildcard_match(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|i| wildcard_match(rest, &name[i..])),
        Some((b'?', rest)) => !name.is_empty() && wildcard_match(rest, &name[1..]),
        Some((b'[', rest)) => {
            let Some(close) = rest.iter().position(|&b| b == b']') else {
                // No closing bracket: treat `[` literally.
                return name.first() == Some(&b'[') && wildcard_match(rest, &name[1..]);
            };
            let Some((&c, name_rest)) = name.split_first() else {
                return false;
            };
            let class = &rest[..close];
            let mut matched = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    matched |= class[i] <= c && c <= class[i + 2];
                    i += 3;
                } else {
                    matched |= class[i] == c;
                    i += 1;
                }
            }
            matched && wildcard_match(&rest[close + 1..], name_rest)
        }
        Some((&p, rest)) => name.first() == Some(&p) && wildcard_match(rest, &name[1..]),
    }
}

fn has_wildcard(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Expands an absolute pattern into the existing paths it matches. Wildcards
/// work within one name; there is no `**`.
///
/// Symlinks in the literal start of the pattern are followed (a symlinked
/// `~/.cache` is fine), but after the first wildcard only real folders are
/// entered, so a link can't steer a wildcard somewhere unexpected.
pub fn glob(pattern: &str) -> Vec<PathBuf> {
    let path = Path::new(pattern);
    if !path.is_absolute() {
        return Vec::new();
    }
    let parts: Vec<&std::ffi::OsStr> = path.components().skip(1).map(|c| c.as_os_str()).collect();
    let first_wildcard = parts
        .iter()
        .position(|p| has_wildcard(&p.to_string_lossy()));

    let mut current = vec![PathBuf::from("/")];
    for (i, part) in parts.iter().enumerate() {
        let mut next = Vec::new();
        for base in &current {
            if first_wildcard.is_some_and(|w| i > w)
                && !std::fs::symlink_metadata(base).is_ok_and(|m| m.is_dir())
            {
                continue;
            }
            if has_wildcard(&part.to_string_lossy()) {
                let Ok(entries) = std::fs::read_dir(base) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if wildcard_match(part.as_bytes(), entry.file_name().as_bytes()) {
                        next.push(entry.path());
                    }
                }
            } else {
                let candidate = base.join(part);
                if std::fs::symlink_metadata(&candidate).is_ok() {
                    next.push(candidate);
                }
            }
        }
        current = next;
    }
    current.sort();
    current
}

/// Something found by a cleaner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Can be cleaned now.
    Ready,
    /// Nothing found.
    Empty,
    /// One of the cleaner's apps is open; holds the process name.
    AppRunning(String),
    /// Only the administrator can clean this.
    NeedsRoot,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub cleaner: Cleaner,
    pub items: Vec<Item>,
    pub size: u64,
    pub files: u64,
    pub status: Status,
    /// Extra information to show, e.g. what's being kept.
    pub note: Option<String>,
}

impl Finding {
    /// Can be selected for cleaning (now, or with the administrator
    /// password).
    pub fn can_clean(&self) -> bool {
        matches!(self.status, Status::Ready | Status::NeedsRoot)
    }

    pub fn needs_root(&self) -> bool {
        self.status == Status::NeedsRoot
    }
}

/// The administrator task that cleans a root-only cleaner.
pub fn root_operation(cleaner: &Cleaner, keep_package_versions: u32) -> Option<Operation> {
    match (cleaner.kind, cleaner.id.as_str()) {
        (Kind::PackageCache, _) => Some(Operation::CleanPackageCache {
            keep: keep_package_versions.min(10),
        }),
        (Kind::Journal, _) => Some(Operation::VacuumJournal { days: 14 }),
        (Kind::Paths, "coredumps") => Some(Operation::CleanCoredumps),
        (Kind::Paths, "old-logs") => Some(Operation::CleanOldLogs),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    pub keep_package_versions: u32,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            keep_package_versions: 2,
        }
    }
}

/// Measures what each cleaner would remove. Removes nothing. Runs in
/// parallel and blocks, so call it from a background thread.
pub fn analyze(
    cleaners: &[Cleaner],
    places: &Places,
    guard: &PathGuard,
    running: &HashSet<String>,
    options: &AnalyzeOptions,
) -> Vec<Finding> {
    cleaners
        .par_iter()
        .map(|c| analyze_one(c, cleaners, places, guard, running, options))
        .collect()
}

fn analyze_one(
    cleaner: &Cleaner,
    all: &[Cleaner],
    places: &Places,
    guard: &PathGuard,
    running: &HashSet<String>,
    options: &AnalyzeOptions,
) -> Finding {
    let (mut paths, note) = match cleaner.kind {
        Kind::Paths => (path_candidates(cleaner, all, places), None),
        Kind::PackageCache => package_cache_candidates(options.keep_package_versions),
        Kind::Journal => {
            let journal = PathBuf::from("/var/log/journal");
            (
                if journal.is_dir() {
                    vec![journal]
                } else {
                    Vec::new()
                },
                Some("Cleaning keeps the most recent two weeks of logs.".to_owned()),
            )
        }
    };

    // System locations are only measured here; the helper cleans them, so
    // the home-folder guard doesn't apply to them.
    if !cleaner.needs_root {
        paths.retain(|p| guard.check(p).is_ok());
    }

    let items: Vec<Item> = paths
        .par_iter()
        .map(|p| {
            let (size, files) = measure(p);
            Item {
                path: p.clone(),
                size,
                files,
            }
        })
        .filter(|i| i.size > 0 || i.files > 0)
        .collect();

    let size = items.iter().map(|i| i.size).sum();
    let files = items.iter().map(|i| i.files).sum();
    let status = if items.is_empty() {
        Status::Empty
    } else if cleaner.needs_root {
        Status::NeedsRoot
    } else if let Some(name) = cleaner.running.iter().find(|n| running.contains(*n)) {
        Status::AppRunning(name.clone())
    } else {
        Status::Ready
    };

    let mut items = items;
    items.sort_by_key(|i| std::cmp::Reverse(i.size));
    Finding {
        cleaner: cleaner.clone(),
        items,
        size,
        files,
        status,
        note,
    }
}

fn path_candidates(cleaner: &Cleaner, all: &[Cleaner], places: &Places) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for pattern in &cleaner.paths {
        let expanded = places.expand(pattern);
        let mut matches = glob(&expanded);
        if cleaner.exclude_known {
            let claimed = claimed_names(&expanded, cleaner, all, places);
            matches.retain(|m| {
                let name = m.file_name().map(|n| n.as_bytes()).unwrap_or_default();
                name != crate::APP_DIR_NAME.as_bytes()
                    && !claimed.iter().any(|c| wildcard_match(c.as_bytes(), name))
            });
        }
        found.extend(matches);
    }
    found.sort();
    found.dedup();
    // Drop anything inside another match, so nothing is counted twice.
    let mut result: Vec<PathBuf> = Vec::new();
    for p in found {
        if !result.iter().any(|r| p.starts_with(r)) {
            result.push(p);
        }
    }
    result
}

/// For a pattern like `/home/me/.cache/*`, the names right under
/// `/home/me/.cache` that other cleaners' paths go into.
fn claimed_names(pattern: &str, me: &Cleaner, all: &[Cleaner], places: &Places) -> Vec<String> {
    let Some(base) = pattern.strip_suffix("/*") else {
        return Vec::new();
    };
    let prefix = format!("{base}/");
    all.iter()
        .filter(|c| c.id != me.id && c.kind == Kind::Paths)
        .flat_map(|c| c.paths.iter().map(|p| places.expand(p)))
        .filter_map(|p| {
            p.strip_prefix(&prefix)
                .and_then(|rest| rest.split('/').next())
                .map(str::to_owned)
        })
        .collect()
}

/// Size on disk and file count of a file or folder.
pub fn measure(path: &Path) -> (u64, u64) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if !meta.is_dir() {
        return (meta.blocks() * 512, 1);
    }
    let options = ScanOptions {
        files_per_folder: 0,
        largest_files: 0,
        ..ScanOptions::default()
    };
    match scan(path, &options, &ScanProgress::default()) {
        Ok(r) => (r.root.size, r.root.file_count),
        Err(_) => (meta.blocks() * 512, 0),
    }
}

/// Name, version and architecture from a pacman package file name such as
/// `firefox-133.0-1-x86_64.pkg.tar.zst`.
pub fn parse_pacman_package(file_name: &str) -> Option<(&str, &str, &str)> {
    let (stem, _) = file_name.split_once(".pkg.tar")?;
    let mut parts = stem.rsplitn(4, '-');
    let arch = parts.next()?;
    let rel = parts.next()?;
    let ver = parts.next()?;
    let name = parts.next()?;
    let ver_rel = &stem[name.len() + 1..stem.len() - arch.len() - 1];
    debug_assert!(ver_rel.ends_with(rel) && ver_rel.starts_with(ver));
    (!name.is_empty()).then_some((name, ver_rel, arch))
}

/// Picks package files to remove so that only the newest `keep` versions of
/// each package stay. Input: `(file name, modified time)`. Newer is decided
/// by modified time, which follows install order.
pub fn old_package_versions(files: &[(String, i64)], keep: u32) -> Vec<String> {
    let mut groups: BTreeMap<(&str, &str), Vec<(&str, i64)>> = BTreeMap::new();
    for (name, mtime) in files {
        if name.ends_with(".sig") || name.ends_with(".part") {
            continue;
        }
        if let Some((pkg, _, arch)) = parse_pacman_package(name) {
            groups.entry((pkg, arch)).or_default().push((name, *mtime));
        }
    }
    let mut old = Vec::new();
    for versions in groups.values_mut() {
        versions.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
        for (name, _) in versions.iter().skip(keep as usize) {
            old.push((*name).to_owned());
            let sig = format!("{name}.sig");
            if files.iter().any(|(f, _)| *f == sig) {
                old.push(sig);
            }
        }
    }
    old.sort();
    old
}

fn package_cache_candidates(keep: u32) -> (Vec<PathBuf>, Option<String>) {
    let pacman = Path::new("/var/cache/pacman/pkg");
    if pacman.is_dir() {
        let files: Vec<(String, i64)> = std::fs::read_dir(pacman)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                meta.is_file()
                    .then(|| (e.file_name().to_string_lossy().into_owned(), meta.mtime()))
            })
            .collect();
        let old = old_package_versions(&files, keep);
        let note = match keep {
            0 => "Removes every cached package.".to_owned(),
            1 => "Keeps the newest version of each package.".to_owned(),
            n => format!("Keeps the newest {n} versions of each package."),
        };
        return (
            old.into_iter().map(|f| pacman.join(f)).collect(),
            Some(note),
        );
    }

    // Other package managers: the whole cache can go.
    for dir in [
        "/var/cache/apt/archives",
        "/var/cache/libdnf5",
        "/var/cache/dnf",
        "/var/cache/zypp/packages",
    ] {
        let dir = Path::new(dir);
        if dir.is_dir() {
            let items = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| !p.ends_with("lock") && !p.ends_with("partial"))
                .collect();
            return (items, None);
        }
    }
    (Vec::new(), None)
}

/// Names of running programs (from `/proc/*/comm` and the program path), used
/// to skip cleaners whose app is open.
pub fn running_processes() -> HashSet<String> {
    let mut names = HashSet::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return names;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let dir = entry.path();
        if let Ok(comm) = std::fs::read_to_string(dir.join("comm")) {
            names.insert(comm.trim().to_owned());
        }
        if let Ok(cmdline) = std::fs::read(dir.join("cmdline"))
            && let Some(argv0) = cmdline.split(|&b| b == 0).next()
            && let Some(base) = Path::new(std::ffi::OsStr::from_bytes(argv0)).file_name()
        {
            names.insert(base.to_string_lossy().into_owned());
        }
    }
    names.remove("");
    names
}

/// Live counters for a GUI while cleaning.
#[derive(Debug, Default)]
pub struct CleanProgress {
    done: AtomicU64,
    total: AtomicU64,
    cancelled: AtomicBool,
    waiting_for_admin: AtomicBool,
}

impl CleanProgress {
    pub fn done(&self) -> u64 {
        self.done.load(Relaxed)
    }
    pub fn total(&self) -> u64 {
        self.total.load(Relaxed)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Relaxed);
    }
    /// Set while the administrator password prompt is open.
    pub fn set_waiting_for_admin(&self, waiting: bool) {
        self.waiting_for_admin.store(waiting, Relaxed);
    }
    pub fn waiting_for_admin(&self) -> bool {
        self.waiting_for_admin.load(Relaxed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CleanReport {
    /// Bytes actually freed.
    pub freed: u64,
    /// Items removed.
    pub removed: u64,
    /// Items that couldn't be removed, with the reason.
    pub failures: Vec<(PathBuf, String)>,
    /// Cleaners skipped because their app was opened in the meantime.
    pub skipped_running: Vec<String>,
}

/// Deletes the items of `findings` that are ready to clean. Caches are
/// deleted permanently: moving them to the Trash would free no space.
pub fn clean(findings: &[Finding], guard: &PathGuard, progress: &CleanProgress) -> CleanReport {
    let mut report = CleanReport::default();
    let running = running_processes();
    let todo: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.status == Status::Ready)
        .filter(|f| {
            let open = f.cleaner.running.iter().any(|n| running.contains(n));
            if open {
                report.skipped_running.push(f.cleaner.name.clone());
            }
            !open
        })
        .collect();
    progress
        .total
        .store(todo.iter().map(|f| f.items.len() as u64).sum(), Relaxed);

    for item in todo.iter().flat_map(|f| &f.items) {
        if progress.cancelled.load(Relaxed) {
            break;
        }
        match remove(&item.path, DeleteMode::Permanent, guard) {
            Ok(()) => {
                report.freed += item.size;
                report.removed += 1;
            }
            Err(RemoveError::NotFound(_)) => {}
            Err(e) => {
                // A folder may be partly removed; count what went.
                let (left, _) = measure(&item.path);
                report.freed += item.size.saturating_sub(left);
                report.failures.push((item.path.clone(), e.to_string()));
            }
        }
        progress.done.fetch_add(1, Relaxed);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn builtin_rules_parse() {
        let cleaners = builtin_cleaners();
        assert!(cleaners.len() > 20);
        assert!(
            cleaners
                .iter()
                .any(|c| c.id == "thumbnails" && c.default_on)
        );
        // Things you can't get back are never checked by default.
        for id in ["trash", "recent-files", "other-app-caches"] {
            let c = cleaners.iter().find(|c| c.id == id).unwrap();
            assert!(!c.default_on, "{id} must not be on by default");
        }
        // Every path is absolute after expansion and inside a known place,
        // unless the cleaner needs root.
        let places = test_places(Path::new("/home/t"));
        for c in &cleaners {
            for p in &c.paths {
                let e = places.expand(p);
                assert!(e.starts_with('/'), "{}: {e}", c.id);
                assert!(c.needs_root || e.starts_with("/home/t/"), "{}: {e}", c.id);
            }
        }
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let text = r#"
[[cleaner]]
id = "a"
name = "A"
category = "apps"
description = "x"
[[cleaner]]
id = "a"
name = "A again"
category = "apps"
description = "y"
"#;
        assert!(matches!(parse_rules(text), Err(RuleError::DuplicateId(_))));
        assert!(parse_rules("[[cleaner]]\nid = 1\n").is_err());
    }

    #[test]
    fn wildcards() {
        let m = |p: &str, n: &str| wildcard_match(p.as_bytes(), n.as_bytes());
        assert!(m("*", "anything"));
        assert!(m("*", ".hidden"));
        assert!(m("*.gz", "syslog.2.gz"));
        assert!(!m("*.gz", "syslog.2"));
        assert!(m("google-chrome*", "google-chrome-beta"));
        assert!(m("?.txt", "a.txt"));
        assert!(!m("?.txt", "ab.txt"));
        assert!(m("*.[0-9]", "messages.1"));
        assert!(!m("*.[0-9]", "messages.x"));
        assert!(m("[ab]c", "bc"));
        assert!(m("a[", "a["));
        assert!(m("Code Cache", "Code Cache"));
    }

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lcc-junk-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, rel: &str, bytes: usize) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, vec![1u8; bytes]).unwrap();
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_places(home: &Path) -> Places {
        Places {
            home: home.to_path_buf(),
            cache: home.join(".cache"),
            config: home.join(".config"),
            data: home.join(".local/share"),
            state: home.join(".local/state"),
        }
    }

    fn rule(id: &str, paths: &[&str]) -> Cleaner {
        Cleaner {
            id: id.to_owned(),
            name: id.to_owned(),
            category: Category::Apps,
            description: String::new(),
            icon: None,
            default_on: true,
            running: Vec::new(),
            paths: paths.iter().map(|s| (*s).to_owned()).collect(),
            kind: Kind::Paths,
            needs_root: false,
            exclude_known: false,
        }
    }

    #[test]
    fn glob_expands_wildcards_and_skips_symlinked_folders() {
        let t = Temp::new("glob");
        t.file("a/p1/cache2/x", 10);
        t.file("a/p2 (release)/cache2/y", 10);
        t.file("a/p3/other/z", 10);
        // A link matched by a wildcard is not entered...
        std::os::unix::fs::symlink(t.0.join("a"), t.0.join("b")).unwrap();
        // ...but a link in the literal start of the pattern is followed.
        std::os::unix::fs::symlink(t.0.join("a"), t.0.join("a-link")).unwrap();

        let base = t.0.display().to_string();
        let found = glob(&format!("{base}/a/*/cache2"));
        assert_eq!(
            found,
            vec![t.0.join("a/p1/cache2"), t.0.join("a/p2 (release)/cache2")]
        );
        assert_eq!(
            glob(&format!("{base}/*/p1/cache2")),
            vec![t.0.join("a/p1/cache2")]
        );
        assert_eq!(glob(&format!("{base}/a-link/*/cache2")).len(), 2);
        assert!(glob("relative/*").is_empty());
    }

    #[test]
    fn analyze_measures_and_respects_guard_and_running_apps() {
        let t = Temp::new("analyze");
        let home = &t.0;
        t.file(".cache/thumbnails/normal/a.png", 8192);
        t.file(".cache/thumbnails/large/b.png", 8192);
        t.file(".cache/browser/cache2/c", 8192);
        t.file(".cache/misc/d", 8192);
        t.file(".cache/linux-crap-cleaner/e", 8192);
        t.file(".ssh/id_ed25519", 100);

        let places = test_places(home);
        let mut browser = rule("browser", &["{cache}/browser/cache2"]);
        browser.running = vec!["browserd".to_owned()];
        let mut other = rule("other", &["{cache}/*"]);
        other.exclude_known = true;
        let sneaky = rule("sneaky", &["{home}/.ssh/*", "{home}/.cache"]);
        let cleaners = vec![
            rule("thumbs", &["{cache}/thumbnails/*"]),
            browser,
            other,
            sneaky,
        ];

        let guard = PathGuard::new(home, &[]);
        let running: HashSet<String> = ["browserd".to_owned()].into();
        let findings = analyze(
            &cleaners,
            &places,
            &guard,
            &running,
            &AnalyzeOptions::default(),
        );

        let thumbs = &findings[0];
        assert_eq!(thumbs.status, Status::Ready);
        assert_eq!(thumbs.items.len(), 2);
        assert_eq!(thumbs.files, 2);
        assert!(thumbs.size >= 2 * 8192);

        assert_eq!(
            findings[1].status,
            Status::AppRunning("browserd".to_owned())
        );

        // "Other" skips folders other rules handle, and our own folder.
        let other_paths: Vec<_> = findings[2].items.iter().map(|i| i.path.clone()).collect();
        assert_eq!(other_paths, vec![home.join(".cache/misc")]);

        // Protected paths never show up.
        assert_eq!(findings[3].status, Status::Empty);
    }

    #[test]
    fn clean_removes_ready_items_only() {
        let t = Temp::new("clean");
        let home = &t.0;
        t.file(".cache/thumbnails/normal/a.png", 8192);
        t.file(".cache/app/x", 8192);
        let places = test_places(home);
        let mut busy = rule("busy", &["{cache}/app"]);
        busy.running = vec!["appd".to_owned()];
        let cleaners = vec![rule("thumbs", &["{cache}/thumbnails/*"]), busy];
        let guard = PathGuard::new(home, &[]);
        let running: HashSet<String> = ["appd".to_owned()].into();
        let findings = analyze(
            &cleaners,
            &places,
            &guard,
            &running,
            &AnalyzeOptions::default(),
        );

        let progress = CleanProgress::default();
        let report = clean(&findings, &guard, &progress);
        assert_eq!(report.removed, 1);
        assert!(report.freed >= 8192);
        assert!(report.failures.is_empty());
        assert_eq!(progress.done(), 1);
        assert!(!home.join(".cache/thumbnails/normal").exists());
        assert!(
            home.join(".cache/thumbnails").exists(),
            "the folder itself stays"
        );
        assert!(
            home.join(".cache/app/x").exists(),
            "running app's cache is kept"
        );
    }

    #[test]
    fn every_root_cleaner_has_an_operation() {
        for c in builtin_cleaners().iter().filter(|c| c.needs_root) {
            assert!(
                root_operation(c, 2).is_some(),
                "{} has no administrator task",
                c.id
            );
        }
        let cache = builtin_cleaners()
            .into_iter()
            .find(|c| c.id == "package-cache")
            .unwrap();
        assert_eq!(
            root_operation(&cache, 99),
            Some(Operation::CleanPackageCache { keep: 10 })
        );
    }

    #[test]
    fn pacman_file_names() {
        assert_eq!(
            parse_pacman_package("firefox-133.0-1-x86_64.pkg.tar.zst"),
            Some(("firefox", "133.0-1", "x86_64"))
        );
        assert_eq!(
            parse_pacman_package("lib32-mesa-1:25.1.0-2-x86_64.pkg.tar.zst.sig"),
            Some(("lib32-mesa", "1:25.1.0-2", "x86_64"))
        );
        assert_eq!(
            parse_pacman_package("python-foo-bar-1.2-3-any.pkg.tar.xz"),
            Some(("python-foo-bar", "1.2-3", "any"))
        );
        assert_eq!(parse_pacman_package("notapackage.txt"), None);
        assert_eq!(parse_pacman_package("x-1-any.pkg.tar.zst"), None);
    }

    #[test]
    fn keeps_newest_package_versions() {
        let files: Vec<(String, i64)> = [
            ("foo-1.0-1-x86_64.pkg.tar.zst", 100),
            ("foo-1.0-1-x86_64.pkg.tar.zst.sig", 100),
            ("foo-1.1-1-x86_64.pkg.tar.zst", 200),
            ("foo-1.2-1-x86_64.pkg.tar.zst", 300),
            ("bar-2.0-1-any.pkg.tar.zst", 50),
            ("download-abc.part", 400),
        ]
        .iter()
        .map(|(n, t)| ((*n).to_owned(), *t))
        .collect();

        assert_eq!(
            old_package_versions(&files, 2),
            vec![
                "foo-1.0-1-x86_64.pkg.tar.zst".to_owned(),
                "foo-1.0-1-x86_64.pkg.tar.zst.sig".to_owned()
            ]
        );
        assert_eq!(old_package_versions(&files, 1).len(), 3);
        assert_eq!(old_package_versions(&files, 0).len(), 5);
    }

    #[test]
    fn finds_this_process() {
        let names = running_processes();
        assert!(!names.is_empty());
    }
}
