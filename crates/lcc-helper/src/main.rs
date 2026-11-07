//! Privileged helper for Linux Crap Cleaner.
//!
//! The GUI never runs as root. For tasks that need it, the GUI runs this
//! helper through `pkexec`, which asks for the administrator password.
//!
//! The helper accepts only the named operations in
//! [`lcc_core::privileged::Operation`], checked by
//! [`lcc_core::privileged::parse_args`]; never a path or a command. Each
//! operation re-checks its inputs as root, removes packages only through
//! pacman, and prints one report line (`ok`/`error`, operation, bytes
//! freed) that the GUI reads.
//!
//! Exit codes: 0 all done, 1 an operation failed, 2 bad arguments, 3 not
//! running as root.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use lcc_core::junk::{glob, measure, old_package_versions};
use lcc_core::packages::{PACMAN_DB, pacman_orphans, read_pacman_db};
use lcc_core::privileged::{
    OpReport, Operation, delete_files_inside, parse_args, protected_package, running_kernel_package,
};

const PACMAN: &str = "/usr/bin/pacman";
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
                 remove-orphans PKG... | remove-packages PKG... | clean-coredumps | clean-old-logs"
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
            let db = read_pacman_db(Path::new(PACMAN_DB));
            let orphans: Vec<&str> = pacman_orphans(&db)
                .iter()
                .map(|e| e.name.as_str())
                .collect();
            if let Some(p) = packages.iter().find(|p| !orphans.contains(&p.as_str())) {
                return Err(format!(
                    "{p} is no longer an orphan package; nothing was removed"
                ));
            }
            remove_packages(packages)
        }
        Operation::RemovePackages { packages } => {
            let kernel = running_kernel_package();
            let db = read_pacman_db(Path::new(PACMAN_DB));
            for p in packages {
                if !db.iter().any(|e| &e.name == p) {
                    return Err(format!("{p} isn't installed"));
                }
                if let Some(why) = protected_package(p, kernel.as_deref()) {
                    return Err(format!("{p} can't be removed: {why}"));
                }
            }
            remove_packages(packages)
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

/// Removes packages with `pacman -Rns`; returns the installed size that went
/// away (including dependencies only they needed).
fn remove_packages(packages: &[String]) -> Result<u64, String> {
    if Path::new(PACMAN_LOCK).exists() {
        return Err(busy());
    }
    let db = Path::new(PACMAN_DB);
    let before: HashMap<String, u64> = read_pacman_db(db)
        .into_iter()
        .map(|e| (e.name, e.size))
        .collect();
    let mut args = vec!["-Rns", "--noconfirm", "--"];
    args.extend(packages.iter().map(String::as_str));
    command(PACMAN, &args)?;
    let after: Vec<String> = read_pacman_db(db).into_iter().map(|e| e.name).collect();
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
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("couldn't run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(lcc_core::privileged::explain_pacman_error(&stderr))
}

fn log_errors(errors: &[String]) {
    for e in errors {
        eprintln!("lcc-helper: {e}");
    }
}
