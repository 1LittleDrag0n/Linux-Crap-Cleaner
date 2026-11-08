//! Safety rules that every delete in the app must pass.
//!
//! [`PathGuard::check`] is the single gate in front of anything that removes
//! or trashes a file. It refuses:
//!
//! * relative paths and paths containing `..`
//! * system locations (`/usr`, `/etc`, `/boot`, ...), which are only ever
//!   changed through the package manager or the privileged helper
//! * private data in the home folder (`~/.ssh`, `~/.gnupg`, keyrings, ...)
//! * the home folder itself and the standard folders inside it (Documents,
//!   Downloads, ...), though files inside them may be removed
//! * browser files holding passwords, cookies or history
//! * anything inside a folder the user excluded in Settings
//! * the top folder of a mounted drive or subvolume, and the folders drives
//!   are mounted in (`/media`, `/mnt`, `/run/media/<user>`); files on the
//!   drive may be removed
//!
//! Paths are checked both as written and with their parent folder's symlinks
//! resolved, so `~/link-to-usr/bin` is refused too.

use std::path::{Component, Path, PathBuf};

/// Why a path may not be deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protected {
    pub path: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for Protected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is protected: {}", self.path.display(), self.reason)
    }
}

impl std::error::Error for Protected {}

/// A built-in rule, shown read-only in Settings so users can see what the app
/// will never touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub path: PathBuf,
    pub reason: &'static str,
    /// `true`: only the folder itself is protected, files inside may go.
    /// `false`: the folder and everything inside it is protected.
    pub exact_only: bool,
}

/// System folders that are only changed through the package manager or the
/// privileged helper.
const SYSTEM_TREES: &[(&str, &str)] = &[
    ("/bin", "system programs"),
    ("/boot", "boot loader and kernels"),
    ("/dev", "device files"),
    ("/efi", "boot loader"),
    ("/etc", "system configuration"),
    ("/lib", "system libraries"),
    ("/lib32", "system libraries"),
    ("/lib64", "system libraries"),
    ("/opt", "installed applications"),
    ("/proc", "kernel interface"),
    ("/root", "administrator's home folder"),
    ("/run", "runtime state"),
    ("/sbin", "system programs"),
    ("/snap", "installed Snap packages"),
    ("/srv", "server data"),
    ("/sys", "kernel interface"),
    ("/usr", "installed software"),
    ("/var/lib", "package databases and service data"),
];

/// Where udisks mounts removable drives: `/run/media/<user>/<drive>`.
const REMOVABLE_MEDIA: &str = "/run/media";
const DRIVES_FOLDER: &str = "the folder drives are mounted in";

/// Private data inside the home folder.
const PRIVATE_HOME_TREES: &[(&str, &str)] = &[
    (".ssh", "SSH keys"),
    (".gnupg", "GPG keys"),
    (".password-store", "saved passwords"),
    (".pki", "certificates"),
    (".local/share/keyrings", "saved passwords"),
    (".local/share/kwalletd", "saved passwords"),
    (".config/Bitwarden", "saved passwords"),
    (".config/KeePassXC", "password manager settings"),
];

/// Folders inside home that must not be removed themselves, though their
/// contents may be cleaned.
const HOME_FOLDERS: &[&str] = &[
    ".cache",
    ".config",
    ".local",
    ".local/share",
    ".local/state",
    ".local/bin",
    "Desktop",
    "Documents",
    "Downloads",
    "Music",
    "Pictures",
    "Public",
    "Templates",
    "Videos",
];

/// Browser files holding passwords, cookies or history, protected wherever
/// they appear.
const SENSITIVE_FILE_NAMES: &[&str] = &[
    // Firefox and forks
    "logins.json",
    "logins-backup.json",
    "key4.db",
    "cert9.db",
    "cookies.sqlite",
    "places.sqlite",
    "formhistory.sqlite",
    // Chromium-based browsers
    "Login Data",
    "Login Data For Account",
    "Cookies",
    "History",
    "Web Data",
    "Local State",
];

/// Decides whether a path may be deleted.
#[derive(Debug, Clone)]
pub struct PathGuard {
    trees: Vec<(PathBuf, String)>,
    exact: Vec<(PathBuf, String)>,
}

impl PathGuard {
    /// Builds the guard for a given home folder plus the user's excluded
    /// folders from Settings.
    pub fn new(home: &Path, excluded: &[PathBuf]) -> Self {
        let mut trees: Vec<(PathBuf, String)> = SYSTEM_TREES
            .iter()
            .map(|(p, why)| (PathBuf::from(p), (*why).to_owned()))
            .collect();
        trees.extend(
            PRIVATE_HOME_TREES
                .iter()
                .map(|(p, why)| (home.join(p), (*why).to_owned())),
        );
        trees.extend(
            excluded
                .iter()
                .map(|p| (normalize(p), "excluded in Settings".to_owned())),
        );

        let mut exact = vec![
            (PathBuf::from("/"), "the root of the system".to_owned()),
            (
                PathBuf::from("/home"),
                "contains every user's home folder".to_owned(),
            ),
            (PathBuf::from("/var"), "system data".to_owned()),
            (PathBuf::from("/tmp"), "shared temporary folder".to_owned()),
            (PathBuf::from("/media"), DRIVES_FOLDER.to_owned()),
            (PathBuf::from("/mnt"), DRIVES_FOLDER.to_owned()),
            (home.to_path_buf(), "your home folder".to_owned()),
        ];
        exact.extend(
            HOME_FOLDERS
                .iter()
                .map(|p| (home.join(p), "a standard folder".to_owned())),
        );

        Self { trees, exact }
    }

    /// Builds the guard for the current user.
    pub fn for_current_user(excluded: &[PathBuf]) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/nonexistent-home"));
        Self::new(&home, excluded)
    }

    /// Returns `Ok(())` when `path` may be deleted, or the reason it may not.
    ///
    /// This does not follow a symlink at `path` itself: deleting a symlink
    /// removes the link, never its target. Callers must use
    /// `symlink_metadata` and never recurse through symlinks.
    pub fn check(&self, path: &Path) -> Result<(), Protected> {
        let refuse = |reason: &str| Protected {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        };

        if !path.is_absolute() {
            return Err(refuse("not an absolute path"));
        }
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(refuse("path contains `..`"));
        }

        let lexical = normalize(path);
        self.check_normalized(&lexical).map_err(|r| refuse(&r))?;

        if is_mount_point(&lexical) {
            return Err(refuse("the top folder of a mounted drive"));
        }

        // Resolve symlinks in the parent so a link can't smuggle a protected
        // location past the lexical check.
        if let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name())
            && let Ok(real_parent) = parent.canonicalize()
        {
            let resolved = real_parent.join(name);
            if resolved != lexical {
                self.check_normalized(&resolved).map_err(|r| refuse(&r))?;
            }
        }

        Ok(())
    }

    fn check_normalized(&self, path: &Path) -> Result<(), String> {
        if let Some((_, why)) = self.exact.iter().find(|(p, _)| p == path) {
            return Err(why.clone());
        }
        // Removable drives: `/run/media/<user>/<drive>/...` is the user's
        // data, though the rest of `/run` is runtime state.
        let on_removable_drive = path.starts_with(REMOVABLE_MEDIA) && {
            let depth = path.components().count();
            if depth <= 5 {
                return Err(DRIVES_FOLDER.to_owned());
            }
            true
        };
        if let Some((tree, why)) = self.trees.iter().find(|(p, _)| path.starts_with(p))
            && !(on_removable_drive && tree == Path::new("/run"))
        {
            return Err(why.clone());
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && SENSITIVE_FILE_NAMES.contains(&name)
        {
            return Err("browser passwords, cookies or history".to_owned());
        }
        Ok(())
    }

    /// The built-in rules, for display in Settings. User exclusions are not
    /// included.
    pub fn builtin_rules(home: &Path) -> Vec<Rule> {
        let mut rules: Vec<Rule> = SYSTEM_TREES
            .iter()
            .map(|(p, why)| Rule {
                path: PathBuf::from(p),
                reason: why,
                exact_only: false,
            })
            .collect();
        rules.extend(PRIVATE_HOME_TREES.iter().map(|(p, why)| Rule {
            path: home.join(p),
            reason: why,
            exact_only: false,
        }));
        rules.extend(HOME_FOLDERS.iter().map(|p| Rule {
            path: home.join(p),
            reason: "the folder itself; files inside can be cleaned",
            exact_only: true,
        }));
        rules
    }

    /// File names protected wherever they appear.
    pub fn sensitive_file_names() -> &'static [&'static str] {
        SENSITIVE_FILE_NAMES
    }
}

/// A path on a drive other than the ones holding the system and the home
/// folder: anything under `/mnt`, `/media` or `/run/media`, or on a real disk
/// that isn't the system's or home's. Subvolumes and partitions of the
/// system disk (`/var/cache` on its own btrfs subvolume) don't count, nor do
/// memory filesystems like `/tmp`.
pub fn on_other_drive(path: &Path, home: &Path, mounts: &[crate::disks::Mount]) -> bool {
    use crate::disks::{is_disk_fs, mount_for};
    if ["/mnt", "/media", REMOVABLE_MEDIA]
        .iter()
        .any(|p| path.starts_with(p))
    {
        return true;
    }
    let Some(mount) = mount_for(mounts, path) else {
        return false;
    };
    if !is_disk_fs(&mount.fs_type) {
        return false;
    }
    let source_of = |p: &Path| mount_for(mounts, p).map(|m| m.source.as_str());
    Some(mount.source.as_str()) != source_of(Path::new("/"))
        && Some(mount.source.as_str()) != source_of(home)
}

/// A folder on a different filesystem than its parent: a mounted drive, a
/// partition or a btrfs subvolume. Deleting it would empty the whole drive.
fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    match (std::fs::symlink_metadata(path), std::fs::metadata(parent)) {
        (Ok(meta), Ok(parent_meta)) => meta.is_dir() && meta.dev() != parent_meta.dev(),
        _ => false,
    }
}

/// Removes `.` components and trailing slashes without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| *c != Component::CurDir)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> PathGuard {
        PathGuard::new(
            Path::new("/home/test"),
            &[PathBuf::from("/home/test/Projects/keep")],
        )
    }

    fn allowed(p: &str) -> bool {
        guard().check(Path::new(p)).is_ok()
    }

    #[test]
    fn refuses_relative_and_parent_paths() {
        assert!(!allowed("relative/file"));
        assert!(!allowed("/home/test/.cache/../.ssh/id_ed25519"));
    }

    #[test]
    fn refuses_system_locations() {
        for p in [
            "/",
            "/usr",
            "/usr/lib/libc.so.6",
            "/etc/fstab",
            "/boot/vmlinuz-linux",
            "/var/lib/pacman",
        ] {
            assert!(!allowed(p), "{p} should be protected");
        }
    }

    #[test]
    fn allows_system_caches_for_the_helper() {
        assert!(allowed(
            "/var/cache/pacman/pkg/foo-1.0-1-x86_64.pkg.tar.zst"
        ));
        assert!(allowed("/var/log/old.log.1"));
        assert!(allowed("/tmp/leftover"));
    }

    #[test]
    fn refuses_home_and_standard_folders_but_allows_contents() {
        assert!(!allowed("/home/test"));
        assert!(!allowed("/home/test/"));
        assert!(!allowed("/home/test/Downloads"));
        assert!(!allowed("/home/test/.cache"));
        assert!(allowed("/home/test/Downloads/big.iso"));
        assert!(allowed("/home/test/.cache/thumbnails"));
    }

    #[test]
    fn refuses_private_data() {
        assert!(!allowed("/home/test/.ssh"));
        assert!(!allowed("/home/test/.ssh/id_ed25519"));
        assert!(!allowed("/home/test/.gnupg/pubring.kbx"));
        assert!(!allowed("/home/test/.local/share/keyrings/login.keyring"));
    }

    #[test]
    fn refuses_browser_secrets_anywhere() {
        assert!(!allowed(
            "/home/test/.mozilla/firefox/abc.default/logins.json"
        ));
        assert!(!allowed("/home/test/.config/chromium/Default/Login Data"));
        assert!(!allowed("/home/test/.config/chromium/Default/Cookies"));
        assert!(allowed(
            "/home/test/.cache/mozilla/firefox/abc.default/cache2/entries/X"
        ));
    }

    #[test]
    fn refuses_user_exclusions() {
        assert!(!allowed("/home/test/Projects/keep"));
        assert!(!allowed("/home/test/Projects/keep/target/debug"));
        assert!(allowed("/home/test/Projects/other/target"));
    }

    #[test]
    fn prefix_match_is_by_component_not_by_text() {
        // `/usrlocal` is not inside `/usr`.
        assert!(allowed("/usrlocal/junk"));
        assert!(allowed("/home/test/.sshfoo"));
    }

    #[test]
    fn allows_files_on_removable_drives() {
        assert!(allowed("/run/media/test/HDD1/fiverr/p2"));
        assert!(allowed("/run/media/test/HDD1/old.iso"));
        assert!(!allowed("/run/media/test/HDD1"));
        assert!(!allowed("/run/media/test"));
        assert!(!allowed("/run/media"));
        assert!(!allowed("/run/user/1000/bus"));
        assert!(!allowed("/media"));
        assert!(!allowed("/mnt"));
        assert!(allowed("/mnt/backup/old"));
        // Exclusions and browser secrets still apply on drives.
        assert!(!allowed("/run/media/test/HDD1/firefox/logins.json"));
    }

    #[test]
    fn knows_other_drives() {
        let mounts = crate::disks::parse_mountinfo(
            "1 0 0:1 / / rw - btrfs /dev/nvme0n1p2 rw\n\
             2 1 0:2 / /home rw - btrfs /dev/nvme0n1p2 rw\n\
             3 1 0:3 / /var/cache rw - btrfs /dev/nvme0n1p2 rw\n\
             4 1 0:4 / /tmp rw - tmpfs tmpfs rw\n\
             5 1 0:5 / /data rw - ext4 /dev/sdb1 rw\n",
        );
        let home = Path::new("/home/test");
        let other = |p: &str| on_other_drive(Path::new(p), home, &mounts);
        assert!(other("/data/old"));
        assert!(other("/mnt/linuxp1/stuff"));
        assert!(other("/run/media/test/HDD1/fiverr"));
        assert!(!other("/home/test/Downloads/x"));
        assert!(!other("/var/cache/pacman"));
        assert!(!other("/tmp/x"));
    }

    #[test]
    fn refuses_mount_points() {
        // `/proc` is always a different filesystem from `/`.
        let g = PathGuard::new(Path::new("/home/test"), &[]);
        assert!(is_mount_point(Path::new("/proc")));
        assert!(!is_mount_point(Path::new("/proc/self/fd/0")));
        assert!(g.check(Path::new("/proc")).is_err());
    }

    #[test]
    fn dot_components_are_ignored() {
        assert!(!allowed("/home/test/./.ssh/config"));
    }

    #[test]
    fn symlinked_parent_is_resolved() {
        let dir = std::env::temp_dir().join(format!("lcc-safety-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("link-to-etc");
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        let g = PathGuard::new(Path::new("/home/test"), &[]);
        let result = g.check(&link.join("fstab"));
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(
            result.is_err(),
            "a symlink to /etc must not bypass the guard"
        );
    }

    #[test]
    fn builtin_rules_are_listed() {
        let rules = PathGuard::builtin_rules(Path::new("/home/test"));
        assert!(
            rules
                .iter()
                .any(|r| r.path == Path::new("/usr") && !r.exact_only)
        );
        assert!(
            rules
                .iter()
                .any(|r| r.path == Path::new("/home/test/Downloads") && r.exact_only)
        );
    }
}
