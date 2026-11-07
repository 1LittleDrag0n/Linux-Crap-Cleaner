//! Duplicates, big files and empty folders.
//!
//! [`find`] walks the chosen folders once, in parallel, and produces:
//!
//! * groups of identical files, compared by content in three steps (the
//!   approach Czkawka and fclones use): same size → same hash of the first
//!   16 KiB → same BLAKE3 hash of the whole file. Hard links to one file are
//!   never reported as duplicates of each other, since removing one frees
//!   nothing.
//! * the largest files
//! * empty folders (only the top folder of an empty tree is reported)
//!
//! Symlinks are never followed; hidden files are skipped unless asked for;
//! memory-only filesystems, snapshots and (optionally) other disks are not
//! entered. Nothing is removed here.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::safety::PathGuard;
use crate::scan::{folders_not_to_enter, is_snapshot_dir};

const PARTIAL_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct FindOptions {
    pub roots: Vec<PathBuf>,
    /// Smaller files are ignored for duplicates.
    pub min_size: u64,
    pub include_hidden: bool,
    pub excluded: Vec<PathBuf>,
    pub stay_on_one_filesystem: bool,
    /// Files at least this big are listed as big files.
    pub big_file_min: u64,
    /// At most this many big files are listed.
    pub big_files_max: usize,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            min_size: 100_000,
            include_hidden: false,
            excluded: Vec::new(),
            stay_on_one_filesystem: true,
            big_file_min: 50_000_000,
            big_files_max: 200,
        }
    }
}

/// What the finder is doing, for a progress display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Listing,
    Comparing,
}

#[derive(Debug, Default)]
pub struct FindProgress {
    stage: AtomicU8,
    files: AtomicU64,
    to_hash: AtomicU64,
    hashed: AtomicU64,
    cancelled: AtomicBool,
}

impl FindProgress {
    pub fn stage(&self) -> Stage {
        if self.stage.load(Relaxed) == 0 {
            Stage::Listing
        } else {
            Stage::Comparing
        }
    }
    /// Files seen while listing.
    pub fn files(&self) -> u64 {
        self.files.load(Relaxed)
    }
    /// Bytes to read while comparing, and bytes read so far.
    pub fn bytes_to_compare(&self) -> u64 {
        self.to_hash.load(Relaxed)
    }
    pub fn bytes_compared(&self) -> u64 {
        self.hashed.load(Relaxed)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Relaxed);
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub size: u64,
    pub modified: i64,
    /// Inside a software project (a folder with `.git`, `Cargo.toml`,
    /// `package.json`, ...). Never picked by automatic selection.
    pub in_project: bool,
}

/// Files or folders that mark a software project.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "CMakeLists.txt",
    "meson.build",
    "Makefile",
    "build.gradle",
    "pom.xml",
    "composer.json",
    "Gemfile",
    "mix.exs",
    "flake.nix",
];

/// Files with identical content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub size: u64,
    pub files: Vec<FileInfo>,
}

impl DuplicateGroup {
    /// Space freed by keeping only one copy.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len().saturating_sub(1)) as u64
    }
}

#[derive(Debug, Clone, Default)]
pub struct FindResult {
    /// Largest waste first.
    pub duplicates: Vec<DuplicateGroup>,
    /// Largest first.
    pub big_files: Vec<FileInfo>,
    pub empty_folders: Vec<PathBuf>,
    pub files_checked: u64,
    pub elapsed: Duration,
}

impl FindResult {
    /// Updates the result after `path` was removed from disk.
    pub fn forget(&mut self, path: &Path) {
        for g in &mut self.duplicates {
            g.files.retain(|f| f.path != path);
        }
        self.duplicates.retain(|g| g.files.len() > 1);
        self.big_files.retain(|f| f.path != path);
        self.empty_folders.retain(|p| p != path);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FindError {
    #[error("no folders to search")]
    NoFolders,
    #[error("search cancelled")]
    Cancelled,
}

/// Which copy to keep when selecting duplicates automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepRule {
    Newest,
    Oldest,
    /// The copy with the shortest path, usually the "original" location.
    ShortestPath,
}

/// The copies `rule` would remove from `group`.
///
/// Files inside software projects are never picked: removing them could break
/// the project. Among the other copies one is always kept, so this removes
/// nothing when at most one copy is outside a project.
pub fn copies_to_remove(group: &DuplicateGroup, rule: KeepRule) -> Vec<&Path> {
    let outside: Vec<&FileInfo> = group.files.iter().filter(|f| !f.in_project).collect();
    let keep = match rule {
        KeepRule::Newest => outside
            .iter()
            .max_by_key(|f| (f.modified, Reverse(&f.path))),
        KeepRule::Oldest => outside.iter().min_by_key(|f| (f.modified, &f.path)),
        KeepRule::ShortestPath => outside
            .iter()
            .min_by_key(|f| (f.path.as_os_str().len(), &f.path)),
    };
    outside
        .iter()
        .filter(|f| Some(*f) != keep)
        .map(|f| f.path.as_path())
        .collect()
}

/// A file found while walking, with what's needed to spot hard links.
#[derive(Debug, Clone)]
struct Found {
    info: FileInfo,
    dev: u64,
    ino: u64,
}

struct Walk<'a> {
    options: &'a FindOptions,
    progress: &'a FindProgress,
    skip: HashSet<PathBuf>,
    guard: &'a PathGuard,
}

struct WalkResult {
    files: Vec<Found>,
    empty_folders: Vec<PathBuf>,
    /// The folder holds nothing at all (at any depth).
    empty: bool,
}

impl Walk<'_> {
    fn walk(&self, dir: &Path, hidden_above: bool, project_above: bool) -> WalkResult {
        let mut result = WalkResult {
            files: Vec::new(),
            empty_folders: Vec::new(),
            empty: false,
        };
        if self.progress.is_cancelled() {
            return result;
        }
        let Ok(read) = std::fs::read_dir(dir) else {
            return result;
        };
        let read: Vec<std::fs::DirEntry> = read.flatten().collect();
        let in_project = project_above
            || read
                .iter()
                .any(|e| PROJECT_MARKERS.contains(&&*e.file_name().to_string_lossy()));

        let mut entries = 0usize;
        let mut subdirs = Vec::new();
        for entry in read {
            entries += 1;
            let name = entry.file_name();
            let hidden = name.as_bytes().first() == Some(&b'.');
            if hidden && !self.options.include_hidden {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                if !self.skip.contains(&path)
                    && !is_snapshot_dir(&path)
                    && !self.options.excluded.iter().any(|e| e == &path)
                {
                    subdirs.push((path, hidden_above || hidden));
                }
            } else if ft.is_file()
                && let Ok(meta) = entry.metadata()
            {
                self.progress.files.fetch_add(1, Relaxed);
                result.files.push(Found {
                    info: FileInfo {
                        path,
                        size: meta.len(),
                        modified: meta.mtime(),
                        in_project,
                    },
                    dev: meta.dev(),
                    ino: meta.ino(),
                });
            }
        }

        let children: Vec<(PathBuf, bool, WalkResult)> = subdirs
            .into_par_iter()
            .map(|(path, hidden)| {
                let r = self.walk(&path, hidden, in_project);
                (path, hidden, r)
            })
            .collect();

        let all_children_empty = children.iter().all(|(_, _, r)| r.empty);
        // Only the raw entry count matters: a folder holding just a hidden
        // file is not empty, whatever the hidden-files setting.
        let subdir_count = children.len();
        result.empty = entries == subdir_count && all_children_empty;

        for (path, hidden, child) in children {
            if child.empty {
                // Report the top of an empty tree, outside hidden folders and
                // software projects (build tools expect their empty
                // folders), and never a protected folder such as ~/Templates.
                if !result.empty && !hidden && !in_project && self.guard.check(&path).is_ok() {
                    result.empty_folders.push(path);
                }
            } else {
                result.empty_folders.extend(child.empty_folders);
            }
            result.files.extend(child.files);
        }
        result
    }
}

/// Runs the search. Blocks; call from a background thread.
pub fn find(
    options: &FindOptions,
    guard: &PathGuard,
    progress: &FindProgress,
) -> Result<FindResult, FindError> {
    let started = Instant::now();
    let roots: Vec<&PathBuf> = options.roots.iter().filter(|r| r.is_dir()).collect();
    if roots.is_empty() {
        return Err(FindError::NoFolders);
    }

    // 1. List files.
    let mut files: Vec<Found> = Vec::new();
    let mut empty_folders = Vec::new();
    for root in &roots {
        let walk = Walk {
            options,
            progress,
            skip: folders_not_to_enter(root, options.stay_on_one_filesystem),
            guard,
        };
        let r = walk.walk(root, false, false);
        files.extend(r.files);
        empty_folders.extend(r.empty_folders);
    }
    if progress.is_cancelled() {
        return Err(FindError::Cancelled);
    }

    // The same file reached twice (overlapping roots) counts once.
    let mut seen_paths = HashSet::new();
    files.retain(|f| seen_paths.insert(f.info.path.clone()));
    let files_checked = files.len() as u64;

    // Big files.
    let mut big_files: Vec<FileInfo> = files
        .iter()
        .filter(|f| f.info.size >= options.big_file_min && guard.check(&f.info.path).is_ok())
        .map(|f| f.info.clone())
        .collect();
    big_files.sort_by_key(|f| Reverse(f.size));
    big_files.truncate(options.big_files_max);

    // 2. Candidates: same size, not just hard links of one file.
    let mut by_size: HashMap<u64, Vec<Found>> = HashMap::new();
    for f in files {
        if f.info.size >= options.min_size.max(1) {
            by_size.entry(f.info.size).or_default().push(f);
        }
    }
    let mut candidates: Vec<Vec<Found>> = by_size
        .into_values()
        .map(|mut group| {
            let mut inodes = HashSet::new();
            group.retain(|f| inodes.insert((f.dev, f.ino)));
            group
        })
        .filter(|g| g.len() > 1)
        .collect();

    progress.stage.store(1, Relaxed);
    let partial_bytes: u64 = candidates
        .iter()
        .flatten()
        .map(|f| f.info.size.min(PARTIAL_BYTES as u64))
        .sum();
    progress.to_hash.store(partial_bytes, Relaxed);

    // 3. Same start of file.
    candidates = regroup(candidates, progress, |f| {
        hash_file(&f.info.path, Some(PARTIAL_BYTES), progress)
    });
    if progress.is_cancelled() {
        return Err(FindError::Cancelled);
    }

    // 4. Same whole content. Files no bigger than the partial read are
    // already fully compared.
    let full_bytes: u64 = candidates
        .iter()
        .flatten()
        .filter(|f| f.info.size > PARTIAL_BYTES as u64)
        .map(|f| f.info.size)
        .sum();
    progress.to_hash.fetch_add(full_bytes, Relaxed);
    candidates = regroup(candidates, progress, |f| {
        if f.info.size <= PARTIAL_BYTES as u64 {
            Some([0; 32])
        } else {
            hash_file(&f.info.path, None, progress)
        }
    });
    if progress.is_cancelled() {
        return Err(FindError::Cancelled);
    }

    let mut duplicates: Vec<DuplicateGroup> = candidates
        .into_iter()
        .map(|g| {
            let mut files: Vec<FileInfo> = g.into_iter().map(|f| f.info).collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            DuplicateGroup {
                size: files[0].size,
                files,
            }
        })
        .collect();
    duplicates.sort_by_key(|g| (Reverse(g.wasted()), g.files[0].path.clone()));
    empty_folders.sort();

    Ok(FindResult {
        duplicates,
        big_files,
        empty_folders,
        files_checked,
        elapsed: started.elapsed(),
    })
}

/// Splits each group by `key`, keeping only sub-groups of two or more.
/// Files whose key can't be computed (unreadable) are dropped.
fn regroup(
    groups: Vec<Vec<Found>>,
    progress: &FindProgress,
    key: impl Fn(&Found) -> Option<[u8; 32]> + Sync,
) -> Vec<Vec<Found>> {
    groups
        .into_par_iter()
        .flat_map_iter(|group| {
            let mut by_key: HashMap<[u8; 32], Vec<Found>> = HashMap::new();
            for f in group {
                if progress.is_cancelled() {
                    break;
                }
                if let Some(k) = key(&f) {
                    by_key.entry(k).or_default().push(f);
                }
            }
            by_key.into_values().filter(|g| g.len() > 1)
        })
        .collect()
}

/// BLAKE3 hash of a file, or of its first `limit` bytes.
fn hash_file(path: &Path, limit: Option<usize>, progress: &FindProgress) -> Option<[u8; 32]> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut remaining = limit.unwrap_or(usize::MAX);
    while remaining > 0 {
        if progress.is_cancelled() {
            return None;
        }
        let want = buf.len().min(remaining);
        let n = file.read(&mut buf[..want]).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        progress.hashed.fetch_add(n as u64, Relaxed);
        remaining -= n;
    }
    Some(*hasher.finalize().as_bytes())
}

/// A short, friendly name for a group: "photo.jpg".
pub fn group_name(group: &DuplicateGroup) -> String {
    group
        .files
        .first()
        .and_then(|f| f.path.file_name())
        .map(OsStr::to_string_lossy)
        .map(|s| s.into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lcc-dup-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, content).unwrap();
            p
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options(root: &Path) -> FindOptions {
        FindOptions {
            roots: vec![root.to_path_buf()],
            min_size: 1,
            big_file_min: 50_000,
            ..FindOptions::default()
        }
    }

    fn guard() -> PathGuard {
        PathGuard::new(Path::new("/home/nobody-test"), &[])
    }

    fn run(opts: &FindOptions) -> FindResult {
        find(opts, &guard(), &FindProgress::default()).unwrap()
    }

    #[test]
    fn finds_identical_files_by_content() {
        let t = Temp::new("basic");
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        t.file("a/photo.jpg", &big);
        t.file("b/photo copy.jpg", &big);
        t.file("c/photo (2).jpg", &big);
        // Same size and same start, different end: not a duplicate.
        let mut tail_differs = big.clone();
        *tail_differs.last_mut().unwrap() ^= 0xff;
        t.file("d/almost.jpg", &tail_differs);
        // Same size, different start.
        let mut head_differs = big.clone();
        head_differs[0] ^= 0xff;
        t.file("e/other.jpg", &head_differs);
        t.file("small1.txt", b"hello");
        t.file("small2.txt", b"hello");
        t.file("unique.txt", b"only me");

        let r = run(&options(&t.0));
        assert_eq!(r.duplicates.len(), 2);
        let biggest = &r.duplicates[0];
        assert_eq!(biggest.files.len(), 3);
        assert_eq!(biggest.wasted(), 200_000);
        assert_eq!(r.duplicates[1].files.len(), 2);
        assert_eq!(r.files_checked, 8);
        // Big files: the five 100 kB files.
        assert_eq!(r.big_files.len(), 5);
    }

    #[test]
    fn hard_links_and_min_size() {
        let t = Temp::new("links");
        let data = vec![7u8; 30_000];
        let a = t.file("a.bin", &data);
        fs::hard_link(&a, t.0.join("a-link.bin")).unwrap();
        std::os::unix::fs::symlink(&a, t.0.join("a-symlink.bin")).unwrap();
        let r = run(&options(&t.0));
        assert!(
            r.duplicates.is_empty(),
            "hard and soft links aren't duplicates"
        );

        t.file("b.bin", &data);
        let r = run(&options(&t.0));
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(r.duplicates[0].files.len(), 2, "one per inode");

        let r = run(&FindOptions {
            min_size: 40_000,
            ..options(&t.0)
        });
        assert!(r.duplicates.is_empty());
    }

    #[test]
    fn hidden_files_are_skipped_unless_asked() {
        let t = Temp::new("hidden");
        t.file("x.txt", b"same");
        t.file(".config/x.txt", b"same");
        assert!(run(&options(&t.0)).duplicates.is_empty());
        let r = run(&FindOptions {
            include_hidden: true,
            ..options(&t.0)
        });
        assert_eq!(r.duplicates.len(), 1);
    }

    #[test]
    fn empty_folders() {
        let t = Temp::new("empty");
        fs::create_dir_all(t.0.join("empty-tree/a/b")).unwrap();
        fs::create_dir_all(t.0.join("empty-tree/c")).unwrap();
        fs::create_dir_all(t.0.join("lonely")).unwrap();
        t.file("full/file.txt", b"x");
        fs::create_dir_all(t.0.join("full/inner-empty")).unwrap();
        t.file("hidden-only/.keep", b"");
        fs::create_dir_all(t.0.join(".secret/empty")).unwrap();
        t.file("project/Cargo.toml", b"[package]");
        fs::create_dir_all(t.0.join("project/target/debug/build/out")).unwrap();

        let r = run(&options(&t.0));
        let mut names: Vec<_> = r
            .empty_folders
            .iter()
            .map(|p| p.strip_prefix(&t.0).unwrap().to_path_buf())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                PathBuf::from("empty-tree"),
                PathBuf::from("full/inner-empty"),
                PathBuf::from("lonely")
            ]
        );
    }

    #[test]
    fn excluded_folders_are_not_searched() {
        let t = Temp::new("excluded");
        t.file("keep/a.txt", b"dup");
        t.file("b.txt", b"dup");
        let r = run(&FindOptions {
            excluded: vec![t.0.join("keep")],
            ..options(&t.0)
        });
        assert!(r.duplicates.is_empty());
    }

    #[test]
    fn keep_rules() {
        let file = |p: &str, m: i64| FileInfo {
            path: PathBuf::from(p),
            size: 10,
            modified: m,
            in_project: false,
        };
        let g = DuplicateGroup {
            size: 10,
            files: vec![
                file("/h/Pictures/a.jpg", 100),
                file("/h/Downloads/old/copy/a.jpg", 50),
                file("/h/Desktop/a (1).jpg", 300),
            ],
        };
        let removed = |rule| -> Vec<String> {
            copies_to_remove(&g, rule)
                .iter()
                .map(|p| p.display().to_string())
                .collect()
        };
        assert_eq!(
            removed(KeepRule::Newest),
            ["/h/Pictures/a.jpg", "/h/Downloads/old/copy/a.jpg"]
        );
        assert_eq!(
            removed(KeepRule::Oldest),
            ["/h/Pictures/a.jpg", "/h/Desktop/a (1).jpg"]
        );
        assert_eq!(
            removed(KeepRule::ShortestPath),
            ["/h/Downloads/old/copy/a.jpg", "/h/Desktop/a (1).jpg"]
        );
        for rule in [KeepRule::Newest, KeepRule::Oldest, KeepRule::ShortestPath] {
            assert_eq!(
                copies_to_remove(&g, rule).len(),
                2,
                "always keeps exactly one"
            );
        }

        // A copy inside a project is never picked, and one copy outside
        // projects is always kept.
        let mut in_repo = file("/h/code/app/assets/a.jpg", 1);
        in_repo.in_project = true;
        let mixed = DuplicateGroup {
            size: 10,
            files: vec![file("/h/Pictures/a.jpg", 100), in_repo.clone()],
        };
        assert!(copies_to_remove(&mixed, KeepRule::ShortestPath).is_empty());
        let three = DuplicateGroup {
            size: 10,
            files: vec![
                file("/h/Pictures/a.jpg", 100),
                file("/h/Downloads/a.jpg", 200),
                in_repo,
            ],
        };
        assert_eq!(
            copies_to_remove(&three, KeepRule::Oldest),
            [Path::new("/h/Downloads/a.jpg")]
        );
    }

    #[test]
    fn forgetting_removed_files() {
        let t = Temp::new("forget");
        let a = t.file("a.txt", b"dup");
        t.file("b.txt", b"dup");
        let mut r = run(&options(&t.0));
        assert_eq!(r.duplicates.len(), 1);
        r.forget(&a);
        assert!(
            r.duplicates.is_empty(),
            "a group of one is no longer a duplicate"
        );
    }

    #[test]
    fn cancel_and_no_folders() {
        assert!(matches!(
            find(&FindOptions::default(), &guard(), &FindProgress::default()),
            Err(FindError::NoFolders)
        ));
        let t = Temp::new("cancel");
        let p = FindProgress::default();
        p.cancel();
        assert!(matches!(
            find(&options(&t.0), &guard(), &p),
            Err(FindError::Cancelled)
        ));
    }
}
