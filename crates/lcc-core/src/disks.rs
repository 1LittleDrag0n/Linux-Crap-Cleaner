//! Mounted disks and their usage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub mount_point: PathBuf,
    /// Device or source, e.g. `/dev/nvme0n1p2`, `tmpfs`, `pool/home`.
    pub source: String,
    pub fs_type: String,
}

/// A real disk (or partition) with its space usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// Friendly name: "System", "Home", "Boot" or the volume name.
    pub label: String,
    pub mount_point: PathBuf,
    pub source: String,
    pub fs_type: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

impl Disk {
    /// Used fraction, 0.0 to 1.0.
    pub fn used_fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f64 / self.total as f64
        }
    }
}

/// Filesystems that hold real files on a disk.
const DISK_FILESYSTEMS: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "fuseblk",
    "zfs", "bcachefs", "jfs", "reiserfs", "hfsplus", "nilfs2",
];

/// Filesystems that exist only in memory or describe the kernel; never scanned.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "ramfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "pstore",
    "efivarfs",
    "bpf",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "rpc_pipefs",
    "nsfs",
    "overlay",
    "squashfs",
    "fuse.portal",
    "fuse.gvfsd-fuse",
    "fuse.xdg-document-portal",
];

pub fn is_pseudo_fs(fs_type: &str) -> bool {
    PSEUDO_FILESYSTEMS.contains(&fs_type)
}

pub fn is_disk_fs(fs_type: &str) -> bool {
    DISK_FILESYSTEMS.contains(&fs_type)
}

/// Reads the current mount table. Returns an empty list if it can't be read.
pub fn read_mounts() -> Vec<Mount> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| parse_mountinfo(&t))
        .unwrap_or_default()
}

/// Parses the format of `/proc/self/mountinfo`:
///
/// `36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue`
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mount_point = left.split(' ').nth(4)?;
            let mut right = right.split(' ');
            let fs_type = right.next()?;
            let source = right.next().unwrap_or("");
            Some(Mount {
                mount_point: PathBuf::from(unescape(mount_point)),
                source: unescape(source),
                fs_type: fs_type.to_owned(),
            })
        })
        .collect()
}

/// Mountinfo escapes space, tab, newline and backslash as `\ooo` octal.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            let oct = &bytes[i + 1..i + 4];
            if oct.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = oct
                    .iter()
                    .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
                if let Ok(v) = u8::try_from(value) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount that contains `path` (the one with the longest matching mount
/// point).
pub fn mount_for<'a>(mounts: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.components().count())
}

/// Mount points strictly inside `path` (not `path` itself), such as another
/// drive mounted in a sub-folder. Deleting `path` would reach into them.
pub fn mounts_inside(mounts: &[Mount], path: &Path) -> Vec<PathBuf> {
    let mut inside: Vec<PathBuf> = mounts
        .iter()
        .filter(|m| m.mount_point != path && m.mount_point.starts_with(path))
        .map(|m| m.mount_point.clone())
        .collect();
    inside.sort();
    inside.dedup();
    inside
}

/// Lists real disks with their usage. A disk mounted in several places (such
/// as btrfs subvolumes for `/` and `/home`) is listed once, under its
/// shortest mount point.
pub fn list_disks() -> Vec<Disk> {
    let home = dirs::home_dir();
    disks_from_mounts(&read_mounts(), home.as_deref(), space)
}

fn disks_from_mounts(
    mounts: &[Mount],
    home: Option<&Path>,
    space: impl Fn(&Path) -> Option<(u64, u64, u64)>,
) -> Vec<Disk> {
    let mut by_source: HashMap<&str, &Mount> = HashMap::new();
    for m in mounts.iter().filter(|m| is_disk_fs(&m.fs_type)) {
        if is_hidden_mount(&m.mount_point) {
            continue;
        }
        by_source
            .entry(m.source.as_str())
            .and_modify(|best| {
                if m.mount_point.components().count() < best.mount_point.components().count() {
                    *best = m;
                }
            })
            .or_insert(m);
    }

    let mut disks: Vec<Disk> = by_source
        .into_values()
        .filter_map(|m| {
            let (total, used, available) = space(&m.mount_point)?;
            (total > 0).then(|| Disk {
                label: label_for(&m.mount_point, home),
                mount_point: m.mount_point.clone(),
                source: m.source.clone(),
                fs_type: m.fs_type.clone(),
                total,
                used,
                available,
            })
        })
        .collect();

    disks.sort_by_key(|d| (sort_rank(&d.mount_point, home), d.mount_point.clone()));
    disks
}

/// Mounts that are implementation details rather than disks a user thinks of.
fn is_hidden_mount(p: &Path) -> bool {
    [
        "/var/lib/docker",
        "/var/lib/containers",
        "/snap",
        "/.snapshots",
        "/run/timeshift",
    ]
    .iter()
    .any(|prefix| p.starts_with(prefix))
}

fn sort_rank(p: &Path, home: Option<&Path>) -> u8 {
    if p == Path::new("/") {
        0
    } else if home.is_some_and(|h| h.starts_with(p)) {
        1
    } else if p.starts_with("/boot") || p.starts_with("/efi") {
        3
    } else {
        2
    }
}

fn label_for(p: &Path, home: Option<&Path>) -> String {
    if p == Path::new("/") {
        return "System".to_owned();
    }
    if home.is_some_and(|h| h.starts_with(p)) {
        return "Home".to_owned();
    }
    if p.starts_with("/boot") || p.starts_with("/efi") {
        return "Boot".to_owned();
    }
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// `(total, used, available)` bytes for the filesystem holding `path`.
pub fn space(path: &Path) -> Option<(u64, u64, u64)> {
    let st = rustix::fs::statvfs(path).ok()?;
    let frsize = st.f_frsize;
    let total = st.f_blocks.saturating_mul(frsize);
    let free = st.f_bfree.saturating_mul(frsize);
    let available = st.f_bavail.saturating_mul(frsize);
    Some((total, total.saturating_sub(free), available))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,subvol=/@
23 22 0:22 / /proc rw,nosuid shared:5 - proc proc rw
24 22 0:23 / /sys rw,nosuid shared:6 - sysfs sysfs rw
30 22 0:31 / /home rw,relatime shared:9 - btrfs /dev/nvme0n1p2 rw,subvol=/@home
31 22 259:1 / /boot rw,relatime shared:10 - vfat /dev/nvme0n1p1 rw
32 22 0:33 / /tmp rw shared:11 - tmpfs tmpfs rw
33 22 8:17 / /run/media/me/My\\040Drive rw shared:12 - exfat /dev/sdb1 rw
34 22 0:40 / /.snapshots rw shared:13 - btrfs /dev/nvme0n1p2 rw,subvol=/@snapshots
";

    #[test]
    fn parses_mountinfo() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(mounts.len(), 8);
        assert_eq!(mounts[0].mount_point, Path::new("/"));
        assert_eq!(mounts[0].fs_type, "btrfs");
        assert_eq!(mounts[0].source, "/dev/nvme0n1p2");
        assert_eq!(mounts[6].mount_point, Path::new("/run/media/me/My Drive"));
        assert_eq!(
            mounts_inside(&mounts, Path::new("/run/media/me")),
            vec![PathBuf::from("/run/media/me/My Drive")]
        );
        assert!(mounts_inside(&mounts, Path::new("/run/media/me/My Drive")).is_empty());
    }

    #[test]
    fn unescapes_octal() {
        assert_eq!(unescape("a\\040b"), "a b");
        assert_eq!(unescape("tab\\011x"), "tab\tx");
        assert_eq!(unescape("back\\134slash"), "back\\slash");
        assert_eq!(unescape("trailing\\"), "trailing\\");
        assert_eq!(unescape("short\\04"), "short\\04");
    }

    #[test]
    fn finds_containing_mount() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(
            mount_for(&mounts, Path::new("/home/me/Downloads"))
                .unwrap()
                .mount_point,
            Path::new("/home")
        );
        assert_eq!(
            mount_for(&mounts, Path::new("/usr/lib"))
                .unwrap()
                .mount_point,
            Path::new("/")
        );
    }

    #[test]
    fn lists_each_disk_once_with_labels() {
        let mounts = parse_mountinfo(SAMPLE);
        let disks = disks_from_mounts(&mounts, Some(Path::new("/home/me")), |_| {
            Some((100, 60, 40))
        });
        let labels: Vec<_> = disks.iter().map(|d| d.label.as_str()).collect();
        // `/` and `/home` share a device, so only "System" appears.
        assert_eq!(labels, ["System", "My Drive", "Boot"]);
        assert!((disks[0].used_fraction() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn separate_home_partition_is_labelled_home() {
        let text = "\
1 0 8:2 / / rw - ext4 /dev/sda2 rw
2 1 8:3 / /home rw - ext4 /dev/sda3 rw
";
        let disks = disks_from_mounts(&parse_mountinfo(text), Some(Path::new("/home/me")), |_| {
            Some((10, 5, 5))
        });
        let labels: Vec<_> = disks.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, ["System", "Home"]);
    }

    #[test]
    fn pseudo_filesystems() {
        assert!(is_pseudo_fs("proc"));
        assert!(is_pseudo_fs("tmpfs"));
        assert!(!is_pseudo_fs("btrfs"));
    }

    #[test]
    fn space_of_root_works() {
        let (total, used, available) = space(Path::new("/")).unwrap();
        assert!(total > 0 && used <= total && available <= total);
    }
}
