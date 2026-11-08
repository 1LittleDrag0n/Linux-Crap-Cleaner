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

use std::path::{Path, PathBuf};

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
    /// Remove old (disabled) snap revisions: `(snap name, revision)`.
    RemoveSnapRevisions { revisions: Vec<(String, u32)> },
    /// Uninstall snaps.
    RemoveSnaps { names: Vec<String> },
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
            Operation::RemoveSnapRevisions { .. } => "remove-snap-revisions",
            Operation::RemoveSnaps { .. } => "remove-snaps",
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
            Operation::RemoveSnapRevisions { revisions } => args.extend(
                revisions
                    .iter()
                    .map(|(name, rev)| crate::snap::revision_arg(name, *rev)),
            ),
            Operation::RemoveSnaps { names } => args.extend(names.iter().cloned()),
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

/// Package names (any package manager): letters, digits and `@._+-`, not starting with
/// `-` or `.`.
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b))
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
            "remove-snap-revisions" => {
                if rest.is_empty() || rest.len() > 500 {
                    return Err(format!("{name}: expected 1 to 500 snap revisions"));
                }
                let revisions = rest
                    .iter()
                    .map(|a| {
                        crate::snap::parse_revision_arg(a)
                            .ok_or_else(|| format!("{name}: invalid snap revision {a:?}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Operation::RemoveSnapRevisions { revisions }
            }
            "remove-snaps" => {
                if rest.is_empty() || rest.len() > 100 {
                    return Err(format!("{name}: expected 1 to 100 snap names"));
                }
                if let Some(bad) = rest.iter().find(|n| !crate::snap::valid_snap_name(n)) {
                    return Err(format!("{name}: invalid snap name {bad:?}"));
                }
                Operation::RemoveSnaps {
                    names: rest.to_vec(),
                }
            }
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

/// First argument that makes the app run the helper instead of the window
/// (used inside an AppImage, where the helper can't be started directly).
pub const HELPER_FLAG: &str = "--lcc-helper";

/// How to start the helper: a program and the arguments that come before the
/// operations.
///
/// * Inside an AppImage the helper lives in a mount only the user can read,
///   so pkexec runs the AppImage file itself with [`HELPER_FLAG`]; the app
///   then hands over to the bundled helper.
/// * A development build uses the helper next to it.
/// * Otherwise the installed helper.
pub fn helper_command() -> Option<(PathBuf, Vec<String>)> {
    if let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from)
        && appimage.is_file()
    {
        return Some((appimage, vec![HELPER_FLAG.to_owned()]));
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("lcc-helper")))
        .filter(|p| p.is_file());
    sibling
        .or_else(|| Some(PathBuf::from(INSTALLED_HELPER)).filter(|p| p.is_file()))
        .map(|p| (p, Vec::new()))
}

/// Runs operations as administrator through `pkexec`. Blocks until done,
/// including while the password prompt is open, so call it off the main
/// thread.
pub fn run_privileged(ops: &[Operation]) -> Result<Vec<OpReport>, String> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    let (helper, prefix) =
        helper_command().ok_or("The administrator helper (lcc-helper) isn't installed.")?;
    let output = crate::host::command("pkexec")
        .arg(&helper)
        .args(prefix)
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
            Operation::RemoveSnapRevisions {
                revisions: vec![("firefox".into(), 5091), ("gnome-42-2204".into(), 176)],
            },
            Operation::RemoveSnaps {
                names: vec!["spotify".into()],
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
            "remove-snap-revisions",
            "remove-snap-revisions firefox",
            "remove-snap-revisions firefox=abc",
            "remove-snap-revisions ../x=1",
            "remove-snaps Firefox",
            "remove-snaps --purge",
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
