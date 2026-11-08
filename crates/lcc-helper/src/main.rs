//! Privileged helper for Linux Crap Cleaner.
//!
//! The GUI never runs as root. For tasks that need it, the GUI runs this
//! helper through `pkexec`, which asks for the administrator password.
//!
//! The helper accepts only the named operations in
//! [`lcc_core::privileged::Operation`], checked by
//! [`lcc_core::privileged::parse_args`]; never a path or a command. Each
//! operation re-checks its inputs as root, removes packages only through the
//! system's package manager (pacman, APT, DNF, Zypper) or `snap`, and prints
//! one report line (`ok`/`error`, operation, bytes freed) that the GUI
//! reads.
//!
//! Exit codes: 0 all done, 1 an operation failed, 2 bad arguments, 3 not
//! running as root.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lcc_core::junk::{glob, measure, old_package_versions};
use lcc_core::native::{Backend, explain_error};
use lcc_core::privileged::{OpReport, Operation, delete_files_inside, parse_args};
use lcc_core::snap;

const PACMAN_CACHE: &str = "/var/cache/pacman/pkg";
const PACMAN_LOCK: &str = "/var/lib/pacman/db.lck";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => {
            println!("lcc-helper {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help") | None => {
            eprintln!(
                "usage: lcc-helper OPERATION [ARGS] [-- OPERATION [ARGS]]...\n\
                 operations: clean-package-cache KEEP | vacuum-journal DAYS |\n\
                 remove-orphans PKG... | remove-packages PKG... | clean-coredumps | clean-old-logs |\n\
                 remove-snap-revisions NAME=REV... | remove-snaps NAME..."
            );
            return ExitCode::from(2);
        }
        _ => {}
    }

    let ops = match parse_args(&args) {
        Ok(ops) => ops,
        Err(e) => {
            eprintln!("lcc-helper: {e}; refusing");
            return ExitCode::from(2);
        }
    };
    if !rustix::process::geteuid().is_root() {
        eprintln!("lcc-helper: must be run as administrator (through pkexec)");
        return ExitCode::from(3);
    }

    let mut failed = false;
    for op in &ops {
        let report = match run(op) {
            Ok(freed) => OpReport {
                operation: op.name().to_owned(),
                freed,
                error: None,
            },
            Err(e) => {
                failed = true;
                OpReport {
                    operation: op.name().to_owned(),
                    freed: 0,
                    error: Some(e),
                }
            }
        };
        println!("{}", report.to_line());
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn run(op: &Operation) -> Result<u64, String> {
    match op {
        Operation::CleanPackageCache { keep } => clean_package_cache(*keep),
        Operation::VacuumJournal { days } => {
            let journal = Path::new("/var/log/journal");
            let before = measure(journal).0;
            command("/usr/bin/journalctl", &[&format!("--vacuum-time={days}d")])?;
            Ok(before.saturating_sub(measure(journal).0))
        }
        Operation::RemoveOrphans { packages } => {
            let backend = Backend::detect().ok_or("no supported package manager found")?;
            let installed = backend.list();
            let orphans: Vec<String> = backend
                .orphans(&installed)
                .into_iter()
                .map(|p| p.name)
                .collect();
            if let Some(p) = packages.iter().find(|p| !orphans.contains(p)) {
                return Err(format!(
                    "{p} is no longer an orphan package; nothing was removed"
                ));
            }
            remove_packages(backend, packages, false)
        }
        Operation::RemovePackages { packages } => {
            let backend = Backend::detect().ok_or("no supported package manager found")?;
            let installed = backend.list();
            for p in packages {
                if !installed.iter().any(|e| &e.name == p) {
                    return Err(format!("{p} isn't installed"));
                }
            }
            remove_packages(backend, packages, true)
        }
        Operation::RemoveSnapRevisions { revisions } => {
            let disabled = snap::disabled_revisions();
            let mut freed = 0;
            for (name, rev) in revisions {
                let Some(entry) = disabled
                    .iter()
                    .find(|s| &s.name == name && s.revision == *rev)
                else {
                    return Err(format!(
                        "{name} revision {rev} is not an old, disabled revision"
                    ));
                };
                let size = entry.size();
                command(
                    "/usr/bin/snap",
                    &["remove", &format!("--revision={rev}"), name],
                )?;
                freed += size;
            }
            Ok(freed)
        }
        Operation::RemoveSnaps { names } => {
            let installed = snap::list_all();
            let mut freed = 0;
            for name in names {
                if snap::PROTECTED_SNAPS.contains(&name.as_str()) {
                    return Err(format!("{name} can't be removed: your system needs it"));
                }
                let revisions: Vec<&snap::SnapEntry> =
                    installed.iter().filter(|s| &s.name == name).collect();
                if revisions.is_empty() {
                    return Err(format!("{name} isn't installed"));
                }
                let size: u64 = revisions.iter().map(|s| s.size()).sum();
                command("/usr/bin/snap", &["remove", name])?;
                freed += size;
            }
            Ok(freed)
        }
        Operation::CleanCoredumps => {
            let mut freed = 0;
            for dir in ["/var/lib/systemd/coredump", "/var/crash"] {
                let dir = Path::new(dir);
                if dir.is_dir() {
                    let files = regular_files_in(dir);
                    freed += delete_files_inside(&files, dir).0;
                }
            }
            Ok(freed)
        }
        Operation::CleanOldLogs => {
            let files: Vec<PathBuf> = [
                "/var/log/*.old",
                "/var/log/*.[0-9]",
                "/var/log/*.gz",
                "/var/log/*/*.gz",
            ]
            .iter()
            .flat_map(|p| glob(p))
            .collect();
            let (freed, errors) = delete_files_inside(&files, Path::new("/var/log"));
            log_errors(&errors);
            Ok(freed)
        }
    }
}

fn clean_package_cache(keep: u32) -> Result<u64, String> {
    let pacman_cache = Path::new(PACMAN_CACHE);
    if pacman_cache.is_dir() {
        if Path::new(PACMAN_LOCK).exists() {
            return Err(busy());
        }
        let files: Vec<(String, i64)> = std::fs::read_dir(pacman_cache)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                meta.is_file()
                    .then(|| (e.file_name().to_string_lossy().into_owned(), meta.mtime()))
            })
            .collect();
        let old: Vec<PathBuf> = old_package_versions(&files, keep)
            .into_iter()
            .map(|f| pacman_cache.join(f))
            .collect();
        let (freed, errors) = delete_files_inside(&old, pacman_cache);
        log_errors(&errors);
        return Ok(freed);
    }
    for (program, args, dir) in [
        (
            "/usr/bin/apt-get",
            &["clean"][..],
            "/var/cache/apt/archives",
        ),
        (
            "/usr/bin/dnf",
            &["clean", "packages"][..],
            "/var/cache/libdnf5",
        ),
        ("/usr/bin/dnf", &["clean", "packages"][..], "/var/cache/dnf"),
        (
            "/usr/bin/zypper",
            &["clean", "--all"][..],
            "/var/cache/zypp/packages",
        ),
    ] {
        let dir = Path::new(dir);
        if Path::new(program).exists() && dir.is_dir() {
            let before = measure(dir).0;
            command(program, args)?;
            return Ok(before.saturating_sub(measure(dir).0));
        }
    }
    Err("no supported package cache found".to_owned())
}

/// Removes packages (and the dependencies only they needed) with the system
/// package manager; returns the installed size that went away.
///
/// Refuses if anything protected would be removed, including packages the
/// removal would take along (APT removes packages that depend on the ones
/// being removed, so its dry run is checked first).
fn remove_packages(
    backend: Backend,
    packages: &[String],
    with_unneeded: bool,
) -> Result<u64, String> {
    if backend.lock_file().is_some_and(|l| Path::new(l).exists()) {
        return Err(busy());
    }
    let installed = backend.list();
    let protected = backend.protected();
    let preview = backend.preview_removal(packages, &installed)?;
    let mut doomed: Vec<&str> = packages.iter().map(String::as_str).collect();
    doomed.extend(preview.packages.iter().map(|(n, _)| n.as_str()));
    if let Some(p) = doomed.iter().find(|p| protected.contains(**p)) {
        return Err(format!(
            "removing this would also remove {p}, which your system needs; nothing was removed"
        ));
    }

    let before: HashMap<String, u64> = installed.into_iter().map(|e| (e.name, e.size)).collect();
    let (program, args) = backend.remove_command(packages, with_unneeded);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    command(program, &args)?;
    let after: Vec<String> = backend.list().into_iter().map(|e| e.name).collect();
    Ok(before
        .iter()
        .filter(|(name, _)| !after.contains(name))
        .map(|(_, size)| size)
        .sum())
}

fn busy() -> String {
    "another package manager is running; try again when it has finished".to_owned()
}

fn regular_files_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .collect()
}

/// Runs a system tool; on failure returns its last error line.
fn command(program: &str, args: &[&str]) -> Result<(), String> {
    let output = lcc_core::host::command(program)
        .args(args)
        .env("LC_ALL", "C")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .output()
        .map_err(|e| format!("couldn't run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(explain_error(&stderr))
}

fn log_errors(errors: &[String]) {
    for e in errors {
        eprintln!("lcc-helper: {e}");
    }
}
