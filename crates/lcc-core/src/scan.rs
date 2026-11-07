//! Fast, parallel folder scanner for the Storage Analyzer.
//!
//! [`scan`] walks a folder tree on all CPU cores and builds a [`DirNode`] tree
//! with the size of every folder. Sizes are space actually used on disk (like
//! `du`), so sparse files count for what they use and hard-linked files are
//! counted once.
//!
//! To keep memory bounded on whole-disk scans, each folder keeps only its
//! largest files individually; the rest are summed into an "other files"
//! bucket. Separately, the largest files of the whole scan are collected into
//! [`ScanResult::largest_files`].
//!
//! The scanner never follows symlinks, skips memory-only filesystems
//! (`/proc`, `/sys`, `tmpfs`, ...) and btrfs snapshot folders, and, if asked,
//! stays on one disk.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::disks::{Mount, is_pseudo_fs, mount_for, read_mounts};
use crate::settings::Settings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// Don't cross into folders where another disk is mounted.
    pub stay_on_one_filesystem: bool,
    /// Include files and folders whose names start with a dot.
    pub include_hidden: bool,
    /// Folders never scanned.
    pub excluded: Vec<PathBuf>,
    /// How many files each folder keeps individually.
    pub files_per_folder: usize,
    /// How many of the scan's largest files to collect.
    pub largest_files: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            stay_on_one_filesystem: true,
            include_hidden: true,
            excluded: Vec::new(),
            files_per_folder: 50,
            largest_files: 200,
        }
    }
}

impl ScanOptions {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            stay_on_one_filesystem: settings.stay_on_one_filesystem,
            include_hidden: settings.show_hidden_files,
            excluded: settings.excluded_paths.clone(),
            ..Self::default()
        }
    }
}

/// Live counters a GUI can poll while a scan runs, and a cancel switch.
#[derive(Debug, Default)]
pub struct ScanProgress {
    files: AtomicU64,
    folders: AtomicU64,
    bytes: AtomicU64,
    cancelled: AtomicBool,
}

impl ScanProgress {
    pub fn files(&self) -> u64 {
        self.files.load(Relaxed)
    }
    pub fn folders(&self) -> u64 {
        self.folders.load(Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Relaxed)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Relaxed)
    }
}

/// Why a folder was listed but not scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Another disk is mounted here.
    OtherDisk,
    /// Excluded in Settings.
    Excluded,
    /// btrfs or Timeshift snapshots; they share space with the live files, so
    /// counting them would wildly overstate usage.
    Snapshots,
}

impl SkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::OtherDisk => "Not scanned: another disk is mounted here",
            SkipReason::Excluded => "Not scanned: excluded in Settings",
            SkipReason::Snapshots => "Not scanned: system snapshots share space with your files",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: OsString,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirNode {
    pub name: OsString,
    /// Total size including everything inside.
    pub size: u64,
    /// Files inside, at any depth.
    pub file_count: u64,
    /// Folders inside, at any depth.
    pub folder_count: u64,
    /// Sub-folders, largest first.
    pub children: Vec<DirNode>,
    /// This folder's largest files, largest first.
    pub files: Vec<FileEntry>,
    /// Files in this folder not kept individually.
    pub other_files_count: u64,
    pub other_files_size: u64,
    /// The folder couldn't be opened (usually needs administrator access).
    pub unreadable: bool,
    pub skipped: Option<SkipReason>,
}

/// One row when listing a folder's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    File,
    /// The "N smaller files" bucket.
    OtherFiles,
}

#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    /// Empty for [`EntryKind::OtherFiles`].
    pub name: &'a OsStr,
    pub size: u64,
    pub kind: EntryKind,
    /// The folder, for [`EntryKind::Folder`].
    pub node: Option<&'a DirNode>,
    /// Files inside (folders), or files in the bucket (other files).
    pub file_count: u64,
}

impl DirNode {
    pub fn child(&self, name: &OsStr) -> Option<&DirNode> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Sub-folders, files and the "other files" bucket, largest first.
    pub fn entries(&self) -> Vec<Entry<'_>> {
        let mut entries: Vec<Entry<'_>> = self
            .children
            .iter()
            .map(|c| Entry {
                name: &c.name,
                size: c.size,
                kind: EntryKind::Folder,
                node: Some(c),
                file_count: c.file_count,
            })
            .chain(self.files.iter().map(|f| Entry {
                name: &f.name,
                size: f.size,
                kind: EntryKind::File,
                node: None,
                file_count: 1,
            }))
            .collect();
        if self.other_files_count > 0 {
            entries.push(Entry {
                name: OsStr::new(""),
                size: self.other_files_size,
                kind: EntryKind::OtherFiles,
                node: None,
                file_count: self.other_files_count,
            });
        }
        entries.sort_by_key(|e| Reverse(e.size));
        entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFile {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub root_path: PathBuf,
    pub root: DirNode,
    /// Largest files anywhere in the scan, largest first.
    pub largest_files: Vec<LargeFile>,
    /// Folders that couldn't be opened.
    pub unreadable_folders: u64,
    pub elapsed: Duration,
}

impl ScanResult {
    /// The folder at `location` (names relative to the scan root).
    pub fn node_at(&self, location: &[OsString]) -> Option<&DirNode> {
        location
            .iter()
            .try_fold(&self.root, |node, name| node.child(name))
    }

    /// Full path of `location`.
    pub fn path_at(&self, location: &[OsString]) -> PathBuf {
        let mut p = self.root_path.clone();
        p.extend(location);
        p
    }

    /// Updates the tree after `path` was removed from disk, so sizes stay
    /// right without rescanning. Returns `false` if `path` wasn't in the tree.
    pub fn remove_path(&mut self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root_path) else {
            return false;
        };
        let parts: Vec<&OsStr> = rel.iter().collect();
        if parts.is_empty() {
            return false;
        }
        let size_hint = self
            .largest_files
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.size);
        let removed = remove_in(&mut self.root, &parts, size_hint).is_some();
        if removed {
            self.largest_files.retain(|f| !f.path.starts_with(path));
        }
        removed
    }
}

/// Removes `parts` below `node`; returns `(size, files, folders)` removed.
fn remove_in(
    node: &mut DirNode,
    parts: &[&OsStr],
    size_hint: Option<u64>,
) -> Option<(u64, u64, u64)> {
    let (name, rest) = parts.split_first()?;
    let removed = if rest.is_empty() {
        if let Some(i) = node.children.iter().position(|c| c.name == *name) {
            let c = node.children.remove(i);
            (c.size, c.file_count, c.folder_count + 1)
        } else if let Some(i) = node.files.iter().position(|f| f.name == *name) {
            let f = node.files.remove(i);
            (f.size, 1, 0)
        } else if let (Some(size), true) = (size_hint, node.other_files_count > 0) {
            // A file kept only in the "other files" bucket.
            let size = size.min(node.other_files_size);
            node.other_files_count -= 1;
            node.other_files_size -= size;
            (size, 1, 0)
        } else {
            return None;
        }
    } else {
        let child = node.children.iter_mut().find(|c| c.name == *name)?;
        remove_in(child, rest, size_hint)?
    };
    node.size = node.size.saturating_sub(removed.0);
    node.file_count = node.file_count.saturating_sub(removed.1);
    node.folder_count = node.folder_count.saturating_sub(removed.2);
    Some(removed)
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("{0} is not a folder")]
    NotAFolder(PathBuf),
    #[error("{0} is not an absolute path")]
    NotAbsolute(PathBuf),
    #[error("could not open {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("scan cancelled")]
    Cancelled,
}

/// Scans `root`. Runs on the rayon thread pool and blocks until done, so call
/// it from a background thread.
pub fn scan(
    root: &Path,
    options: &ScanOptions,
    progress: &ScanProgress,
) -> Result<ScanResult, ScanError> {
    let started = Instant::now();
    if !root.is_absolute() {
        return Err(ScanError::NotAbsolute(root.to_owned()));
    }
    let meta = std::fs::metadata(root).map_err(|source| ScanError::Io {
        path: root.to_owned(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(ScanError::NotAFolder(root.to_owned()));
    }

    let ctx = Ctx::new(root, options, progress, &read_mounts());
    let mut tree = ctx.scan_dir(root, root.as_os_str().to_owned());
    if progress.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    tree.name = root.as_os_str().to_owned();

    let mut largest: Vec<LargeFile> = ctx
        .largest
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|Reverse((size, path))| LargeFile { path, size })
        .collect();
    largest.sort_by_key(|f| Reverse(f.size));

    Ok(ScanResult {
        root_path: root.to_owned(),
        root: tree,
        largest_files: largest,
        unreadable_folders: ctx.unreadable.load(Relaxed),
        elapsed: started.elapsed(),
    })
}

/// Mount points below `root` that a walk must not enter (or must show as
/// skipped). Shared by the Storage Analyzer and the duplicate finder.
fn skip_mounts(
    root: &Path,
    stay_on_one_filesystem: bool,
    mounts: &[Mount],
) -> HashMap<PathBuf, Skip> {
    let mut skip = HashMap::new();
    for p in ["/proc", "/sys", "/dev", "/run"] {
        skip.insert(PathBuf::from(p), Skip::Hide);
    }
    if let Some(root_mount) = mount_for(mounts, root) {
        for m in mounts {
            if m.mount_point == root_mount.mount_point || !m.mount_point.starts_with(root) {
                continue;
            }
            let reason = if is_pseudo_fs(&m.fs_type) {
                Skip::Hide
            } else if is_snapshot_dir(&m.mount_point) {
                Skip::Show(SkipReason::Snapshots)
            } else if stay_on_one_filesystem && m.source != root_mount.source {
                Skip::Show(SkipReason::OtherDisk)
            } else {
                continue;
            };
            skip.insert(m.mount_point.clone(), reason);
        }
    }
    // Never skip the root itself.
    skip.remove(root);
    skip
}

/// Folders below `root` a file walk must not enter: memory-only filesystems,
/// snapshots, and (if asked) other disks.
pub(crate) fn folders_not_to_enter(
    root: &Path,
    stay_on_one_filesystem: bool,
) -> std::collections::HashSet<PathBuf> {
    skip_mounts(root, stay_on_one_filesystem, &read_mounts())
        .into_keys()
        .collect()
}

enum Skip {
    No,
    /// Leave out entirely (memory-only filesystems).
    Hide,
    Show(SkipReason),
}

struct Ctx<'a> {
    options: &'a ScanOptions,
    progress: &'a ScanProgress,
    /// Mount points below the root that must not be entered.
    skip_mounts: HashMap<PathBuf, Skip>,
    seen_hard_links: Mutex<HashSet<(u64, u64)>>,
    largest: Mutex<BinaryHeap<Reverse<(u64, PathBuf)>>>,
    /// Smallest size in a full `largest` heap; smaller files are ignored
    /// without locking.
    largest_floor: AtomicU64,
    unreadable: AtomicU64,
}

impl<'a> Ctx<'a> {
    fn new(
        root: &Path,
        options: &'a ScanOptions,
        progress: &'a ScanProgress,
        mounts: &[Mount],
    ) -> Self {
        let skip_mounts = skip_mounts(root, options.stay_on_one_filesystem, mounts);

        Self {
            options,
            progress,
            skip_mounts,
            seen_hard_links: Mutex::default(),
            largest: Mutex::default(),
            largest_floor: AtomicU64::new(0),
            unreadable: AtomicU64::new(0),
        }
    }

    fn skip(&self, path: &Path) -> Skip {
        if self.options.excluded.iter().any(|e| e == path) {
            return Skip::Show(SkipReason::Excluded);
        }
        if is_snapshot_dir(path) {
            return Skip::Show(SkipReason::Snapshots);
        }
        match self.skip_mounts.get(path) {
            Some(Skip::Hide) => Skip::Hide,
            Some(Skip::Show(r)) => Skip::Show(*r),
            Some(Skip::No) | None => Skip::No,
        }
    }

    fn scan_dir(&self, path: &Path, name: OsString) -> DirNode {
        let mut node = DirNode {
            name,
            ..DirNode::default()
        };
        if self.progress.is_cancelled() {
            return node;
        }
        self.progress.folders.fetch_add(1, Relaxed);

        // The folder's own entry takes a little space too.
        if let Ok(md) = std::fs::symlink_metadata(path) {
            node.size = md.blocks() * 512;
        }

        let reader = match std::fs::read_dir(path) {
            Ok(r) => r,
            Err(_) => {
                node.unreadable = true;
                self.unreadable.fetch_add(1, Relaxed);
                return node;
            }
        };

        let mut subdirs: Vec<(PathBuf, OsString)> = Vec::new();
        let mut files: Vec<FileEntry> = Vec::new();
        for entry in reader.flatten() {
            let name = entry.file_name();
            if !self.options.include_hidden && name.as_bytes().first() == Some(&b'.') {
                continue;
            }
            // `file_type` and `metadata` on a DirEntry don't follow symlinks.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                subdirs.push((entry.path(), name));
                continue;
            }
            let Ok(md) = entry.metadata() else {
                continue;
            };
            let mut size = md.blocks() * 512;
            if md.nlink() > 1 && !self.first_sighting(md.dev(), md.ino()) {
                size = 0;
            }
            self.progress.files.fetch_add(1, Relaxed);
            self.progress.bytes.fetch_add(size, Relaxed);
            self.offer_largest(size, || path.join(&name));
            files.push(FileEntry { name, size });
        }

        let mut children: Vec<DirNode> = subdirs
            .into_par_iter()
            .filter_map(|(p, n)| match self.skip(&p) {
                Skip::Hide => None,
                Skip::Show(reason) => Some(DirNode {
                    name: n,
                    skipped: Some(reason),
                    ..DirNode::default()
                }),
                Skip::No => Some(self.scan_dir(&p, n)),
            })
            .collect();
        children.sort_by_key(|c| Reverse(c.size));
        files.sort_by_key(|f| Reverse(f.size));

        node.size += files.iter().map(|f| f.size).sum::<u64>();
        node.size += children.iter().map(|c| c.size).sum::<u64>();
        node.file_count = files.len() as u64 + children.iter().map(|c| c.file_count).sum::<u64>();
        node.folder_count =
            children.len() as u64 + children.iter().map(|c| c.folder_count).sum::<u64>();

        if files.len() > self.options.files_per_folder {
            let rest = files.split_off(self.options.files_per_folder);
            node.other_files_count = rest.len() as u64;
            node.other_files_size = rest.iter().map(|f| f.size).sum();
        }
        node.files = files;
        node.children = children;
        node
    }

    /// `true` the first time a hard-linked file is seen.
    fn first_sighting(&self, dev: u64, ino: u64) -> bool {
        self.seen_hard_links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((dev, ino))
    }

    fn offer_largest(&self, size: u64, path: impl FnOnce() -> PathBuf) {
        let cap = self.options.largest_files;
        if cap == 0 || size == 0 || size <= self.largest_floor.load(Relaxed) {
            return;
        }
        let mut heap = self.largest.lock().unwrap_or_else(|e| e.into_inner());
        heap.push(Reverse((size, path())));
        if heap.len() > cap {
            heap.pop();
        }
        if heap.len() == cap
            && let Some(Reverse((min, _))) = heap.peek()
        {
            self.largest_floor.store(*min, Relaxed);
        }
    }
}

pub(crate) fn is_snapshot_dir(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new(".snapshots"))
        || path.starts_with("/timeshift")
        || path.starts_with("/timeshift-btrfs")
        || path.starts_with("/run/timeshift")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lcc-scan-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        /// Writes `bytes` of incompressible data, so sizes on disk are
        /// predictable even on compressed btrfs.
        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ bytes as u64;
            let data: Vec<u8> = (0..bytes)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            fs::write(&p, data).unwrap();
            p
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(root: &Path, options: &ScanOptions) -> ScanResult {
        scan(root, options, &ScanProgress::default()).unwrap()
    }

    #[test]
    fn sizes_add_up_and_are_sorted() {
        let t = TempTree::new("sizes");
        t.file("small/a.bin", 64 * 1024);
        t.file("big/deep/b.bin", 512 * 1024);
        t.file("top.bin", 128 * 1024);

        let r = run(&t.0, &ScanOptions::default());
        let root = &r.root;
        assert_eq!(root.file_count, 3);
        assert_eq!(root.folder_count, 3); // small, big, big/deep
        assert_eq!(root.children[0].name, "big");
        assert!(root.children[0].size >= 512 * 1024);
        assert!(root.size >= (64 + 512 + 128) * 1024);

        let parts: u64 = root.children.iter().map(|c| c.size).sum::<u64>()
            + root.files.iter().map(|f| f.size).sum::<u64>();
        assert!(root.size >= parts, "folder size includes its contents");

        let entries = root.entries();
        assert!(entries.windows(2).all(|w| w[0].size >= w[1].size));
        assert_eq!(r.largest_files[0].path, t.0.join("big/deep/b.bin"));
    }

    #[test]
    fn hidden_files_can_be_left_out() {
        let t = TempTree::new("hidden");
        t.file(".cache/x.bin", 64 * 1024);
        t.file("visible.bin", 64 * 1024);

        let with = run(&t.0, &ScanOptions::default());
        let without = run(
            &t.0,
            &ScanOptions {
                include_hidden: false,
                ..ScanOptions::default()
            },
        );
        assert_eq!(with.root.file_count, 2);
        assert_eq!(without.root.file_count, 1);
        assert!(without.root.child(OsStr::new(".cache")).is_none());
    }

    #[test]
    fn excluded_folders_are_listed_but_not_scanned() {
        let t = TempTree::new("excluded");
        t.file("keep/x.bin", 64 * 1024);
        let r = run(
            &t.0,
            &ScanOptions {
                excluded: vec![t.0.join("keep")],
                ..ScanOptions::default()
            },
        );
        let keep = r.root.child(OsStr::new("keep")).unwrap();
        assert_eq!(keep.skipped, Some(SkipReason::Excluded));
        assert_eq!(keep.size, 0);
        assert_eq!(r.root.file_count, 0);
    }

    #[test]
    fn symlinks_are_not_followed() {
        let t = TempTree::new("symlink");
        std::os::unix::fs::symlink("/usr", t.0.join("usr-link")).unwrap();
        let r = run(&t.0, &ScanOptions::default());
        assert!(r.root.children.is_empty());
        assert!(
            r.root.size < 1024 * 1024,
            "the /usr tree must not be counted"
        );
    }

    #[test]
    fn hard_links_are_counted_once() {
        let t = TempTree::new("hardlink");
        let original = t.file("a/original.bin", 256 * 1024);
        fs::create_dir_all(t.0.join("b")).unwrap();
        fs::hard_link(&original, t.0.join("b/link.bin")).unwrap();

        let r = run(&t.0, &ScanOptions::default());
        let a = r.root.child(OsStr::new("a")).unwrap().files[0].size;
        let b = r.root.child(OsStr::new("b")).unwrap().files[0].size;
        assert!(
            (a == 0) != (b == 0),
            "exactly one link carries the size: {a} {b}"
        );
    }

    #[test]
    fn many_files_go_into_other_bucket() {
        let t = TempTree::new("bucket");
        for i in 0..10 {
            t.file(&format!("f{i}.bin"), (i + 1) * 8 * 1024);
        }
        let r = run(
            &t.0,
            &ScanOptions {
                files_per_folder: 3,
                largest_files: 4,
                ..ScanOptions::default()
            },
        );
        assert_eq!(r.root.files.len(), 3);
        assert_eq!(r.root.other_files_count, 7);
        assert_eq!(r.root.file_count, 10);
        assert_eq!(r.largest_files.len(), 4);
        assert_eq!(r.largest_files[0].path, t.0.join("f9.bin"));
        let kinds: Vec<_> = r.root.entries().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EntryKind::OtherFiles));
    }

    #[test]
    fn snapshot_folders_are_skipped() {
        let t = TempTree::new("snapshots");
        t.file(".snapshots/1/snapshot/huge.bin", 256 * 1024);
        let r = run(&t.0, &ScanOptions::default());
        let snap = r.root.child(OsStr::new(".snapshots")).unwrap();
        assert_eq!(snap.skipped, Some(SkipReason::Snapshots));
        assert_eq!(r.root.file_count, 0);
    }

    #[test]
    fn removing_updates_sizes() {
        let t = TempTree::new("remove");
        t.file("a/x.bin", 64 * 1024);
        let y = t.file("a/sub/y.bin", 128 * 1024);
        t.file("b/z.bin", 64 * 1024);

        let mut r = run(&t.0, &ScanOptions::default());
        let before_root = r.root.size;
        let y_size = r.largest_files.iter().find(|f| f.path == y).unwrap().size;

        assert!(r.remove_path(&y));
        assert_eq!(r.root.size, before_root - y_size);
        assert_eq!(r.root.file_count, 2);
        assert!(r.largest_files.iter().all(|f| f.path != y));

        let a_size = r.root.child(OsStr::new("a")).unwrap().size;
        assert!(r.remove_path(&t.0.join("a")));
        assert_eq!(r.root.size, before_root - y_size - a_size);
        assert_eq!(r.root.file_count, 1);
        assert!(r.root.child(OsStr::new("a")).is_none());

        assert!(!r.remove_path(&t.0.join("missing")));
        assert!(!r.remove_path(&t.0));
    }

    #[test]
    fn removing_a_file_from_the_other_bucket() {
        let t = TempTree::new("remove-bucket");
        for i in 0..5 {
            t.file(&format!("f{i}.bin"), (i + 1) * 8 * 1024);
        }
        let mut r = run(
            &t.0,
            &ScanOptions {
                files_per_folder: 1,
                ..ScanOptions::default()
            },
        );
        // f3 is the 2nd largest, so it lives in the bucket.
        assert!(r.remove_path(&t.0.join("f3.bin")));
        assert_eq!(r.root.other_files_count, 3);
        assert_eq!(r.root.file_count, 4);
    }

    #[test]
    fn navigation_helpers() {
        let t = TempTree::new("nav");
        t.file("a/b/c.bin", 8 * 1024);
        let r = run(&t.0, &ScanOptions::default());
        let loc = vec![OsString::from("a"), OsString::from("b")];
        assert_eq!(r.node_at(&loc).unwrap().files[0].name, "c.bin");
        assert_eq!(r.path_at(&loc), t.0.join("a/b"));
        assert!(r.node_at(&[OsString::from("nope")]).is_none());
    }

    #[test]
    fn errors_and_cancel() {
        assert!(matches!(
            scan(
                Path::new("relative"),
                &ScanOptions::default(),
                &ScanProgress::default()
            ),
            Err(ScanError::NotAbsolute(_))
        ));
        let t = TempTree::new("errors");
        let f = t.file("file.bin", 10);
        assert!(matches!(
            scan(&f, &ScanOptions::default(), &ScanProgress::default()),
            Err(ScanError::NotAFolder(_))
        ));
        let progress = ScanProgress::default();
        progress.cancel();
        assert!(matches!(
            scan(&t.0, &ScanOptions::default(), &progress),
            Err(ScanError::Cancelled)
        ));
    }
}
