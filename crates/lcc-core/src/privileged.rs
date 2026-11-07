//! Tasks that need administrator access, run by the `lcc-helper` program.
//!
//! The GUI never runs as root. It starts `lcc-helper` through `pkexec`, which
//! asks for the administrator password, with a short list of named
//! [`Operation`]s. The helper accepts nothing else: no paths, no shell
//! commands. Every operation re-checks its inputs as root, removes packages
//! only through the package manager, and reports how much space it freed.
//!
//! Several operations can run in one call (separated by `--`), so one
//! password prompt covers a whole cleanup.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Packages that must never be removed from the GUI.
pub const CRITICAL_PACKAGES: &[&str] = &[
    "base",
    "base-devel",
    "filesystem",
    "glibc",
    "gcc-libs",
    "bash",
    "coreutils",
    "pacman",
    "pacman-mirrorlist",
    "archlinux-keyring",
    "cachyos-keyring",
    "cachyos-mirrorlist",
    "systemd",
    "systemd-libs",
    "systemd-sysvcompat",
    "linux-firmware",
    "mkinitcpio",
    "dracut",
    "grub",
    "limine",
    "systemd-boot-manager",
    "refind",
    "efibootmgr",
    "sudo",
    "doas",
    "util-linux",
    "shadow",
    "pam",
    "dbus",
    "dbus-broker",
    "polkit",
    "glib2",
    "openssl",
    "zlib",
    "xz",
    "zstd",
    "sed",
    "grep",
    "gawk",
    "tar",
    "gzip",
    "findutils",
    "procps-ng",
    "iproute2",
    "kmod",
    "e2fsprogs",
    "btrfs-progs",
    "xfsprogs",
    "dosfstools",
    "cryptsetup",
    "lvm2",
];

/// One task for the helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Remove old package versions from the cache, keeping the newest `keep`.
    CleanPackageCache { keep: u32 },
    /// Delete journal entries older than `days`.
    VacuumJournal { days: u32 },
    /// Remove packages that are (still) orphans.
    RemoveOrphans { packages: Vec<String> },
    /// Uninstall packages, with the dependencies only they needed.
    RemovePackages { packages: Vec<String> },
    /// Delete saved crash dumps.
    CleanCoredumps,
    /// Delete rotated and compressed logs in /var/log.
    CleanOldLogs,
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Operation::CleanPackageCache { .. } => "clean-package-cache",
            Operation::VacuumJournal { .. } => "vacuum-journal",
            Operation::RemoveOrphans { .. } => "remove-orphans",
            Operation::RemovePackages { .. } => "remove-packages",
            Operation::CleanCoredumps => "clean-coredumps",
            Operation::CleanOldLogs => "clean-old-logs",
        }
    }

    /// Command-line form, as parsed by [`parse_args`].
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![self.name().to_owned()];
        match self {
            Operation::CleanPackageCache { keep } => args.push(keep.to_string()),
            Operation::VacuumJournal { days } => args.push(days.to_string()),
            Operation::RemoveOrphans { packages } | Operation::RemovePackages { packages } => {
                args.extend(packages.iter().cloned())
            }
            Operation::CleanCoredumps | Operation::CleanOldLogs => {}
        }
        args
    }
}

/// Command line for several operations: `op args -- op args ...`.
pub fn ops_to_args(ops: &[Operation]) -> Vec<String> {
    let mut args = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        if i > 0 {
            args.push("--".to_owned());
        }
        args.extend(op.to_args());
    }
    args
}

/// pacman package names: letters, digits and `@._+-`, not starting with
/// `-` or `.`.
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b))
}

/// The package of the running kernel, from `/usr/lib/modules/<release>/pkgbase`.
pub fn running_kernel_package() -> Option<String> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    let base =
        std::fs::read_to_string(format!("/usr/lib/modules/{}/pkgbase", release.trim())).ok()?;
    Some(base.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Why a package may not be removed, if it may not.
pub fn protected_package(name: &str, running_kernel: Option<&str>) -> Option<&'static str> {
    if CRITICAL_PACKAGES.contains(&name) {
        Some("Your system needs it to start and work")
    } else if running_kernel == Some(name)
        || running_kernel.is_some_and(|k| name == format!("{k}-headers"))
    {
        Some("It's the kernel you're running right now")
    } else {
        None
    }
}

/// Parses the helper's command line. Rejects anything unexpected.
pub fn parse_args(args: &[String]) -> Result<Vec<Operation>, String> {
    let mut ops = Vec::new();
    for group in args.split(|a| a == "--") {
        let (name, rest) = group.split_first().ok_or("empty operation")?;
        let number = |min: u32, max: u32| -> Result<u32, String> {
            match rest {
                [n] => n
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| format!("{name}: expected a number from {min} to {max}")),
                _ => Err(format!("{name}: expected one number")),
            }
        };
        let packages = || -> Result<Vec<String>, String> {
            if rest.is_empty() || rest.len() > 500 {
                return Err(format!("{name}: expected 1 to 500 package names"));
            }
            match rest.iter().find(|p| !valid_package_name(p)) {
                Some(bad) => Err(format!("{name}: invalid package name {bad:?}")),
                None => Ok(rest.to_vec()),
            }
        };
        let no_args = |op: Operation| {
            if rest.is_empty() {
                Ok(op)
            } else {
                Err(format!("{name}: takes no arguments"))
            }
        };
        let op = match name.as_str() {
            "clean-package-cache" => Operation::CleanPackageCache {
                keep: number(0, 10)?,
            },
            "vacuum-journal" => Operation::VacuumJournal {
                days: number(1, 3650)?,
            },
            "remove-orphans" => Operation::RemoveOrphans {
                packages: packages()?,
            },
            "remove-packages" => Operation::RemovePackages {
                packages: packages()?,
            },
            "clean-coredumps" => no_args(Operation::CleanCoredumps)?,
            "clean-old-logs" => no_args(Operation::CleanOldLogs)?,
            other => return Err(format!("unknown operation {other:?}")),
        };
        ops.push(op);
    }
    if ops.is_empty() {
        return Err("no operation given".to_owned());
    }
    Ok(ops)
}

/// Result of one operation, as printed by the helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpReport {
    pub operation: String,
    pub freed: u64,
    pub error: Option<String>,
}

impl OpReport {
    /// One line of helper output: `ok<TAB>op<TAB>freed` or
    /// `error<TAB>op<TAB>message`.
    pub fn to_line(&self) -> String {
        match &self.error {
            None => format!("ok\t{}\t{}", self.operation, self.freed),
            Some(e) => format!(
                "error\t{}\t{}",
                self.operation,
                e.replace(['\t', '\n'], " ")
            ),
        }
    }
}

/// Reads the helper's report lines, ignoring anything else it printed.
pub fn parse_report(output: &str) -> Vec<OpReport> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            match (parts.next()?, parts.next()?, parts.next()?) {
                ("ok", op, freed) => Some(OpReport {
                    operation: op.to_owned(),
                    freed: freed.parse().unwrap_or(0),
                    error: None,
                }),
                ("error", op, message) => Some(OpReport {
                    operation: op.to_owned(),
                    freed: 0,
                    error: Some(message.to_owned()),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Where the installed helper lives.
pub const INSTALLED_HELPER: &str = "/usr/lib/linux-crap-cleaner/lcc-helper";

/// Finds the helper: next to the running program (a development build), or
/// the installed one.
pub fn helper_path() -> Option<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("lcc-helper")))
        .filter(|p| p.is_file());
    sibling.or_else(|| Some(PathBuf::from(INSTALLED_HELPER)).filter(|p| p.is_file()))
}

/// Runs operations as administrator through `pkexec`. Blocks until done,
/// including while the password prompt is open, so call it off the main
/// thread.
pub fn run_privileged(ops: &[Operation]) -> Result<Vec<OpReport>, String> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    let helper = helper_path().ok_or("The administrator helper (lcc-helper) isn't installed.")?;
    let output = Command::new("pkexec")
        .arg(&helper)
        .args(ops_to_args(ops))
        .output()
        .map_err(|e| format!("Couldn't ask for the administrator password: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reports = parse_report(&stdout);
    match output.status.code() {
        Some(126) if reports.is_empty() => Err("The password prompt was cancelled.".to_owned()),
        Some(127) if stderr.contains("No authentication agent") => Err(
            "No password prompt is available. Start a polkit authentication agent (for example polkit-gnome or hyprpolkitagent) and try again.".to_owned(),
        ),
        Some(127) if reports.is_empty() => Err("You're not allowed to make system changes.".to_owned()),
        _ if reports.is_empty() => Err(format!(
            "The administrator helper failed: {}",
            stderr.trim().lines().last().unwrap_or("unknown error")
        )),
        _ => Ok(reports),
    }
}

/// Asks pacman (without root) what removing `packages` with `-Rns` would
/// remove: package names and installed sizes. Fails with pacman's message if
/// the removal isn't possible, for example because another package needs
/// one of them.
pub fn preview_package_removal(packages: &[String]) -> Result<Vec<(String, u64)>, String> {
    if packages.iter().any(|p| !valid_package_name(p)) {
        return Err("invalid package name".to_owned());
    }
    let output = Command::new("pacman")
        .args(["-Rnsp", "--print-format", "%n %s", "--"])
        .args(packages)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("couldn't run pacman: {e}"))?;
    if !output.status.success() {
        return Err(explain_pacman_error(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| {
            let (name, size) = l.split_once(' ')?;
            Some((name.to_owned(), size.trim().parse().unwrap_or(0)))
        })
        .collect())
}

/// Turns pacman's dependency errors into a friendly sentence.
pub fn explain_pacman_error(stderr: &str) -> String {
    let needed_by: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.split_once("required by ").map(|(_, who)| who.trim()))
        .collect();
    if !needed_by.is_empty() {
        let mut who: Vec<&str> = needed_by;
        who.sort();
        who.dedup();
        return format!("Other software needs it: {}", who.join(", "));
    }
    stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("pacman refused")
        .trim()
        .trim_start_matches("error: ")
        .to_owned()
}

/// Deletes regular files (never symlinks or folders) that are inside
/// `allowed_root`. Returns bytes freed and errors. Used by the helper.
pub fn delete_files_inside(files: &[PathBuf], allowed_root: &Path) -> (u64, Vec<String>) {
    use std::os::unix::fs::MetadataExt;
    let mut freed = 0;
    let mut errors = Vec::new();
    let Ok(root) = allowed_root.canonicalize() else {
        return (0, vec![format!("{} doesn't exist", allowed_root.display())]);
    };
    for file in files {
        let inside = file
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .is_some_and(|p| p.starts_with(&root));
        let Ok(meta) = std::fs::symlink_metadata(file) else {
            continue;
        };
        if !inside || !meta.is_file() {
            errors.push(format!("skipped {}", file.display()));
            continue;
        }
        match std::fs::remove_file(file) {
            Ok(()) => freed += meta.blocks() * 512,
            Err(e) => errors.push(format!("{}: {e}", file.display())),
        }
    }
    (freed, errors)
}

/// Names of currently installed packages.
pub fn installed_package_names(db: &Path) -> HashSet<String> {
    crate::packages::read_pacman_db(db)
        .into_iter()
        .map(|e| e.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn round_trips_operations() {
        let ops = vec![
            Operation::CleanPackageCache { keep: 2 },
            Operation::VacuumJournal { days: 14 },
            Operation::RemoveOrphans {
                packages: vec!["fontforge".into(), "lib32-foo".into()],
            },
            Operation::CleanCoredumps,
            Operation::CleanOldLogs,
            Operation::RemovePackages {
                packages: vec!["gtk+3".into()],
            },
        ];
        assert_eq!(parse_args(&ops_to_args(&ops)).unwrap(), ops);
    }

    #[test]
    fn rejects_bad_arguments() {
        for bad in [
            "",
            "rm -rf /",
            "clean-package-cache",
            "clean-package-cache 99",
            "clean-package-cache -1",
            "vacuum-journal 0",
            "remove-orphans",
            "remove-orphans --noconfirm",
            "remove-packages ../etc",
            "remove-packages foo;reboot",
            "clean-coredumps /etc",
            "clean-old-logs -- ",
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn package_names() {
        for good in [
            "firefox",
            "lib32-mesa",
            "gtk+3",
            "python3.12",
            "r8168-dkms",
            "@scope",
        ] {
            assert!(valid_package_name(good), "{good}");
        }
        for bad in ["", "-Rdd", ".hidden", "a b", "a/b", "x;y", "$(id)"] {
            assert!(!valid_package_name(bad), "{bad}");
        }
    }

    #[test]
    fn protects_critical_and_running_kernel() {
        assert!(protected_package("glibc", None).is_some());
        assert!(protected_package("linux-cachyos", Some("linux-cachyos")).is_some());
        assert!(protected_package("linux-cachyos-headers", Some("linux-cachyos")).is_some());
        assert!(protected_package("linux-lts", Some("linux-cachyos")).is_none());
        assert!(protected_package("firefox", Some("linux-cachyos")).is_none());
    }

    #[test]
    fn report_lines() {
        let reports = [
            OpReport {
                operation: "clean-package-cache".into(),
                freed: 1234,
                error: None,
            },
            OpReport {
                operation: "remove-orphans".into(),
                freed: 0,
                error: Some("pacman is busy\nretry".into()),
            },
        ];
        let text: String = reports.iter().map(|r| r.to_line() + "\n").collect();
        let noise = format!("some pacman output\n{text}:: done\n");
        let parsed = parse_report(&noise);
        assert_eq!(parsed[0], reports[0]);
        assert_eq!(parsed[1].error.as_deref(), Some("pacman is busy retry"));
    }

    #[test]
    fn friendly_pacman_errors() {
        let stderr = "checking dependencies...\nerror: failed to prepare transaction (could not satisfy dependencies)\n:: removing gtk3 breaks dependency 'gtk3' required by firefox\n:: removing gtk3 breaks dependency 'gtk3' required by gimp\n";
        assert_eq!(
            explain_pacman_error(stderr),
            "Other software needs it: firefox, gimp"
        );
        assert_eq!(
            explain_pacman_error("error: target not found: nope\n"),
            "target not found: nope"
        );
    }

    #[test]
    fn previews_removal_without_root() {
        if !Path::new("/var/lib/pacman/local").is_dir() {
            return;
        }
        // Removing pacman itself is refused by pacman: other packages need it.
        let result = preview_package_removal(&["pacman".to_owned()]);
        assert!(result.is_err());
        assert!(preview_package_removal(&["--help".to_owned()]).is_err());
    }

    #[test]
    fn deletes_only_regular_files_inside_root() {
        let dir = std::env::temp_dir().join(format!("lcc-priv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let inside = dir.join("cache");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("old.pkg"), vec![1u8; 8192]).unwrap();
        std::fs::write(dir.join("outside.txt"), "keep").unwrap();
        std::os::unix::fs::symlink(dir.join("outside.txt"), inside.join("link")).unwrap();

        let (freed, errors) = delete_files_inside(
            &[
                inside.join("old.pkg"),
                inside.join("link"),
                dir.join("outside.txt"),
            ],
            &inside,
        );
        assert!(freed >= 8192);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(!inside.join("old.pkg").exists());
        assert!(dir.join("outside.txt").exists());
        assert!(std::fs::symlink_metadata(inside.join("link")).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
