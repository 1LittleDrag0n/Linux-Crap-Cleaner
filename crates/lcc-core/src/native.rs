//! The system package manager: pacman (Arch, CachyOS, Manjaro, EndeavourOS),
//! APT (Debian, Ubuntu, Mint, Pop!_OS, elementary), DNF (Fedora, RHEL,
//! Alma, Rocky) and Zypper (openSUSE).
//!
//! Every backend answers the same questions: what's installed, which
//! packages are orphans, who owns a file, what would removing a package take
//! with it, which packages must never be removed, and how to remove packages.
//! Reading is done without root, from the package database or a dry run.
//! Removing happens only in the administrator helper.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rayon::prelude::*;

use crate::packages::{self, PACMAN_DB};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Backend {
    Pacman,
    Apt,
    Dnf,
    Zypper,
}

/// One installed package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativePackage {
    pub name: String,
    pub version: String,
    pub description: String,
    pub size: u64,
    pub install_date: Option<i64>,
    /// Installed on purpose, not only as a dependency.
    pub explicit: bool,
}

/// What removing some packages would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// Package names and installed sizes.
    pub packages: Vec<(String, u64)>,
    /// `false` when the package manager can't do a dry run without root
    /// (DNF, Zypper): only the requested packages are listed, and their
    /// unneeded dependencies go too.
    pub complete: bool,
}

/// Packages that must never be removed from the app, on any distribution.
pub const CRITICAL_PACKAGES: &[&str] = &[
    // Everywhere
    "bash",
    "coreutils",
    "filesystem",
    "findutils",
    "gawk",
    "grep",
    "gzip",
    "sed",
    "tar",
    "xz",
    "zstd",
    "zlib",
    "openssl",
    "systemd",
    "systemd-libs",
    "systemd-sysvcompat",
    "systemd-sysv",
    "util-linux",
    "shadow",
    "pam",
    "dbus",
    "dbus-broker",
    "polkit",
    "glib2",
    "kmod",
    "procps-ng",
    "procps",
    "iproute2",
    "sudo",
    "doas",
    "e2fsprogs",
    "btrfs-progs",
    "xfsprogs",
    "dosfstools",
    "cryptsetup",
    "lvm2",
    "efibootmgr",
    "shim",
    "linux-firmware",
    // Arch family
    "base",
    "base-devel",
    "glibc",
    "gcc-libs",
    "pacman",
    "pacman-mirrorlist",
    "archlinux-keyring",
    "cachyos-keyring",
    "cachyos-mirrorlist",
    "manjaro-keyring",
    "endeavouros-keyring",
    "mkinitcpio",
    "dracut",
    "grub",
    "limine",
    "refind",
    "systemd-boot-manager",
    // Debian family
    "apt",
    "dpkg",
    "libc6",
    "libc-bin",
    "base-files",
    "base-passwd",
    "debianutils",
    "init",
    "init-system-helpers",
    "login",
    "passwd",
    "mount",
    "ubuntu-minimal",
    "ubuntu-standard",
    "linux-generic",
    "linux-image-generic",
    "grub-pc",
    "grub-efi-amd64",
    "grub-efi-amd64-signed",
    "grub-common",
    "initramfs-tools",
    "snapd",
    "network-manager",
    // Fedora / openSUSE
    "rpm",
    "dnf",
    "dnf5",
    "dnf-data",
    "libdnf",
    "libdnf5",
    "zypper",
    "libzypp",
    "setup",
    "glibc-common",
    "fedora-release",
    "fedora-release-common",
    "fedora-repos",
    "openSUSE-release",
    "kernel",
    "kernel-core",
    "kernel-modules",
    "kernel-default",
    "grub2-common",
    "grub2-efi-x64",
    "grub2-pc",
    "NetworkManager",
];

fn program_exists(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))
        .chain([PathBuf::from("/usr/bin"), PathBuf::from("/usr/sbin")])
        .any(|d| d.join(name).is_file())
}

/// Runs a read-only command in the C locale and returns its standard output,
/// or `None` if it couldn't run or failed.
fn read_command(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Backend {
    /// The package manager of this system.
    pub fn detect() -> Option<Self> {
        if Path::new(PACMAN_DB).is_dir() && program_exists("pacman") {
            Some(Backend::Pacman)
        } else if Path::new(DPKG_STATUS).is_file() && program_exists("apt-get") {
            Some(Backend::Apt)
        } else if program_exists("dnf") || program_exists("dnf5") {
            Some(Backend::Dnf)
        } else if program_exists("zypper") {
            Some(Backend::Zypper)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Pacman => "pacman",
            Backend::Apt => "APT",
            Backend::Dnf => "DNF",
            Backend::Zypper => "Zypper",
        }
    }

    /// Everything installed.
    pub fn list(self) -> Vec<NativePackage> {
        match self {
            Backend::Pacman => packages::read_pacman_db(Path::new(PACMAN_DB))
                .into_iter()
                .map(|e| NativePackage {
                    name: e.name,
                    version: e.version,
                    description: e.description,
                    size: e.size,
                    install_date: e.install_date,
                    explicit: !e.as_dependency,
                })
                .collect(),
            Backend::Apt => apt_list(),
            Backend::Dnf | Backend::Zypper => {
                let mut list = read_command("rpm", &["-qa", "--qf", RPM_QUERY_FORMAT])
                    .map(|t| parse_rpm_list(&t))
                    .unwrap_or_default();
                let auto = if self == Backend::Dnf {
                    dnf_auto_installed(&list)
                } else {
                    zypper_auto_installed()
                };
                if let Some(auto) = auto {
                    for p in &mut list {
                        p.explicit = !auto.contains(&p.name);
                    }
                }
                list
            }
        }
    }

    /// Packages installed as dependencies that nothing needs any more.
    pub fn orphans(self, installed: &[NativePackage]) -> Vec<NativePackage> {
        let names: HashSet<String> = match self {
            Backend::Pacman => {
                let db = packages::read_pacman_db(Path::new(PACMAN_DB));
                packages::pacman_orphans(&db)
                    .into_iter()
                    .map(|e| e.name.clone())
                    .collect()
            }
            Backend::Apt => read_command("apt-get", &["-s", "autoremove"])
                .map(|t| parse_apt_simulation(&t).into_iter().collect())
                .unwrap_or_default(),
            Backend::Dnf => {
                let program = if program_exists("dnf") { "dnf" } else { "dnf5" };
                read_command(
                    program,
                    &[
                        "-C",
                        "-q",
                        "repoquery",
                        "--unneeded",
                        "--queryformat",
                        "%{name}\\n",
                    ],
                )
                .map(|t| parse_name_list(&t))
                .unwrap_or_default()
            }
            Backend::Zypper => read_command(
                "zypper",
                &[
                    "--non-interactive",
                    "--no-refresh",
                    "packages",
                    "--unneeded",
                ],
            )
            .map(|t| parse_zypper_table(&t).into_iter().collect())
            .unwrap_or_default(),
        };
        installed
            .iter()
            .filter(|p| names.contains(&p.name))
            .cloned()
            .collect()
    }

    /// Which package owns each of `paths`.
    pub fn owners_of(self, paths: &[PathBuf]) -> HashMap<PathBuf, String> {
        if paths.is_empty() {
            return HashMap::new();
        }
        match self {
            Backend::Pacman => file_list_owners(Path::new(PACMAN_DB), paths, |dir| {
                let files = std::fs::read_to_string(dir.join("files")).ok()?;
                let desc = std::fs::read_to_string(dir.join("desc")).ok()?;
                Some((packages::parse_pacman_desc(&desc).name, files, true))
            }),
            Backend::Apt => file_list_owners(Path::new(DPKG_INFO), paths, |file| {
                let name = file.file_name()?.to_str()?.strip_suffix(".list")?;
                let name = name.split(':').next()?.to_owned();
                Some((name, std::fs::read_to_string(file).ok()?, false))
            }),
            Backend::Dnf | Backend::Zypper => rpm_owners(paths),
        }
    }

    /// What removing `names` (and the dependencies only they need) would
    /// remove. Fails with the package manager's reason, for example when
    /// other software needs one of them.
    pub fn preview_removal(
        self,
        names: &[String],
        installed: &[NativePackage],
    ) -> Result<Preview, String> {
        if names
            .iter()
            .any(|n| !crate::privileged::valid_package_name(n))
        {
            return Err("invalid package name".to_owned());
        }
        let sizes: HashMap<&str, u64> = installed
            .iter()
            .map(|p| (p.name.as_str(), p.size))
            .collect();
        let size_of = |n: &str| sizes.get(n).copied().unwrap_or(0);
        match self {
            Backend::Pacman => {
                let mut args = vec!["-Rnsp", "--print-format", "%n %s", "--"];
                args.extend(names.iter().map(String::as_str));
                let out = Command::new("pacman")
                    .args(&args)
                    .env("LC_ALL", "C")
                    .output()
                    .map_err(|e| format!("couldn't run pacman: {e}"))?;
                if !out.status.success() {
                    return Err(explain_error(&String::from_utf8_lossy(&out.stderr)));
                }
                let packages = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|l| {
                        let (name, size) = l.split_once(' ')?;
                        Some((name.to_owned(), size.trim().parse().unwrap_or(0)))
                    })
                    .collect();
                Ok(Preview {
                    packages,
                    complete: true,
                })
            }
            Backend::Apt => {
                let mut args = vec!["-s", "remove", "--autoremove", "--"];
                args.extend(names.iter().map(String::as_str));
                let out = Command::new("apt-get")
                    .args(&args)
                    .env("LC_ALL", "C")
                    .output()
                    .map_err(|e| format!("couldn't run apt-get: {e}"))?;
                if !out.status.success() {
                    return Err(explain_error(&String::from_utf8_lossy(&out.stderr)));
                }
                let packages = parse_apt_simulation(&String::from_utf8_lossy(&out.stdout))
                    .into_iter()
                    .map(|n| {
                        let size = size_of(&n);
                        (n, size)
                    })
                    .collect();
                Ok(Preview {
                    packages,
                    complete: true,
                })
            }
            Backend::Dnf | Backend::Zypper => Ok(Preview {
                packages: names.iter().map(|n| (n.clone(), size_of(n))).collect(),
                complete: false,
            }),
        }
    }

    /// Packages that must never be removed: the critical list, packages the
    /// distribution marks as essential or protected, and the running kernel.
    pub fn protected(self) -> HashSet<String> {
        let mut set: HashSet<String> = CRITICAL_PACKAGES.iter().map(|s| (*s).to_owned()).collect();
        match self {
            Backend::Apt => {
                if let Ok(text) = std::fs::read_to_string(DPKG_STATUS) {
                    set.extend(
                        parse_dpkg_status(&text)
                            .into_iter()
                            .filter(|p| p.essential)
                            .map(|p| p.package.name),
                    );
                }
            }
            Backend::Dnf => {
                for dir in ["/etc/dnf/protected.d", "/usr/share/dnf5/libdnf.conf.d"] {
                    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                        if let Ok(text) = std::fs::read_to_string(e.path()) {
                            set.extend(
                                text.lines()
                                    .map(str::trim)
                                    .filter(|l| {
                                        !l.is_empty() && !l.starts_with('#') && !l.contains('=')
                                    })
                                    .map(str::to_owned),
                            );
                        }
                    }
                }
            }
            Backend::Pacman | Backend::Zypper => {}
        }
        set.extend(self.kernel_package());
        set
    }

    /// The package of the running kernel.
    pub fn kernel_package(self) -> Option<String> {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
        let release = release.trim();
        if self == Backend::Pacman {
            let base =
                std::fs::read_to_string(format!("/usr/lib/modules/{release}/pkgbase")).ok()?;
            return Some(base.trim().to_owned()).filter(|s| !s.is_empty());
        }
        let candidates = [
            PathBuf::from(format!("/boot/vmlinuz-{release}")),
            PathBuf::from(format!("/usr/lib/modules/{release}/vmlinuz")),
            PathBuf::from(format!("/lib/modules/{release}/modules.order")),
        ];
        let existing: Vec<PathBuf> = candidates.into_iter().filter(|p| p.exists()).collect();
        let owners = self.owners_of(&existing);
        existing.iter().find_map(|p| owners.get(p).cloned())
    }

    /// Program and arguments that remove `names`, without asking. Run by the
    /// helper as root. With `with_unneeded`, dependencies nothing else needs
    /// go too. (APT can only do that system-wide, so orphans that were
    /// picked one by one are removed without it.)
    pub fn remove_command(
        self,
        names: &[String],
        with_unneeded: bool,
    ) -> (&'static str, Vec<String>) {
        let mut args: Vec<String> = match self {
            Backend::Pacman => vec!["-Rns".into(), "--noconfirm".into()],
            Backend::Apt if with_unneeded => {
                vec!["remove".into(), "-y".into(), "--autoremove".into()]
            }
            Backend::Apt => vec!["remove".into(), "-y".into()],
            Backend::Dnf => vec!["remove".into(), "-y".into()],
            Backend::Zypper => vec![
                "--non-interactive".into(),
                "remove".into(),
                "--clean-deps".into(),
            ],
        };
        args.push("--".into());
        args.extend(names.iter().cloned());
        let program = match self {
            Backend::Pacman => "/usr/bin/pacman",
            Backend::Apt => "/usr/bin/apt-get",
            Backend::Dnf if Path::new("/usr/bin/dnf").exists() => "/usr/bin/dnf",
            Backend::Dnf => "/usr/bin/dnf5",
            Backend::Zypper => "/usr/bin/zypper",
        };
        (program, args)
    }

    /// File that exists while the package manager is busy.
    pub fn lock_file(self) -> Option<&'static str> {
        match self {
            Backend::Pacman => Some("/var/lib/pacman/db.lck"),
            _ => None,
        }
    }
}

/// Turns a package manager's dependency errors into a friendly sentence.
pub fn explain_error(stderr: &str) -> String {
    let mut needed_by: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.split_once("required by ").map(|(_, who)| who.trim()))
        .collect();
    if !needed_by.is_empty() {
        needed_by.sort();
        needed_by.dedup();
        return format!("Other software needs it: {}", needed_by.join(", "));
    }
    stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("the package manager refused")
        .trim()
        .trim_start_matches("error: ")
        .trim_start_matches("E: ")
        .to_owned()
}

/// Finds owners of `paths` by reading per-package file lists under `root`.
/// `read` gets each entry of `root` and returns the package name, its file
/// list, and whether the list's paths lack the leading `/` (pacman).
fn file_list_owners(
    root: &Path,
    paths: &[PathBuf],
    read: impl Fn(&Path) -> Option<(String, String, bool)> + Sync,
) -> HashMap<PathBuf, String> {
    let wanted: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let Ok(entries) = std::fs::read_dir(root) else {
        return HashMap::new();
    };
    let entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries
        .par_iter()
        .filter_map(|e| read(e))
        .flat_map_iter(|(name, files, relative)| {
            files
                .lines()
                .filter_map(|l| {
                    let path = if relative {
                        PathBuf::from(format!("/{l}"))
                    } else {
                        PathBuf::from(l)
                    };
                    wanted
                        .contains(path.as_path())
                        .then(|| (path, name.clone()))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

// ── APT ─────────────────────────────────────────────────────────────────────

const DPKG_STATUS: &str = "/var/lib/dpkg/status";
const DPKG_INFO: &str = "/var/lib/dpkg/info";
const APT_EXTENDED_STATES: &str = "/var/lib/apt/extended_states";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DpkgEntry {
    pub package: NativePackage,
    pub arch: String,
    pub essential: bool,
}

/// Parses `/var/lib/dpkg/status`, keeping installed packages.
pub fn parse_dpkg_status(text: &str) -> Vec<DpkgEntry> {
    let mut entries = Vec::new();
    for stanza in text.split("\n\n") {
        let mut name = "";
        let mut version = "";
        let mut description = "";
        let mut arch = "";
        let mut size_kib = 0u64;
        let mut installed = false;
        let mut essential = false;
        for line in stanza.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };
            match key {
                "Package" => name = value,
                "Version" => version = value,
                "Architecture" => arch = value,
                "Installed-Size" => size_kib = value.trim().parse().unwrap_or(0),
                "Status" => installed = value.split_whitespace().nth(2) == Some("installed"),
                "Description" => description = value,
                "Essential" => essential |= value == "yes",
                "Priority" => essential |= value == "required",
                _ => {}
            }
        }
        if installed && !name.is_empty() {
            entries.push(DpkgEntry {
                package: NativePackage {
                    name: name.to_owned(),
                    version: version.to_owned(),
                    description: description.to_owned(),
                    size: size_kib * 1024,
                    install_date: None,
                    explicit: true,
                },
                arch: arch.to_owned(),
                essential,
            });
        }
    }
    entries
}

/// Names of automatically installed packages from APT's extended_states.
pub fn parse_apt_extended_states(text: &str) -> HashSet<String> {
    text.split("\n\n")
        .filter(|s| s.lines().any(|l| l.trim() == "Auto-Installed: 1"))
        .filter_map(|s| s.lines().find_map(|l| l.strip_prefix("Package: ")))
        .map(|n| n.trim().to_owned())
        .collect()
}

/// Package names an `apt-get -s` run would remove (`Remv name [version]`).
pub fn parse_apt_simulation(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("Remv "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(|n| n.split(':').next().unwrap_or(n).to_owned())
        .collect()
}

fn apt_list() -> Vec<NativePackage> {
    let Ok(status) = std::fs::read_to_string(DPKG_STATUS) else {
        return Vec::new();
    };
    let auto = std::fs::read_to_string(APT_EXTENDED_STATES)
        .map(|t| parse_apt_extended_states(&t))
        .unwrap_or_default();
    let mut seen = HashSet::new();
    parse_dpkg_status(&status)
        .into_iter()
        // Multi-arch copies (libc6:i386) show once.
        .filter(|e| seen.insert(e.package.name.clone()))
        .map(|e| {
            let mut p = e.package;
            p.explicit = !auto.contains(&p.name);
            p.install_date = [
                format!("{}.list", p.name),
                format!("{}:{}.list", p.name, e.arch),
            ]
            .iter()
            .find_map(|f| std::fs::metadata(Path::new(DPKG_INFO).join(f)).ok())
            .map(|m| m.mtime());
            p
        })
        .collect()
}

// ── RPM (DNF, Zypper) ───────────────────────────────────────────────────────

const RPM_QUERY_FORMAT: &str =
    "%{NAME}\\t%{VERSION}-%{RELEASE}\\t%{SIZE}\\t%{INSTALLTIME}\\t%{SUMMARY}\\n";

/// Parses `rpm -qa --qf "$RPM_QUERY_FORMAT"`.
pub fn parse_rpm_list(text: &str) -> Vec<NativePackage> {
    let mut seen = HashSet::new();
    text.lines()
        .filter_map(|l| {
            let mut c = l.splitn(5, '\t');
            let name = c.next()?.to_owned();
            let version = c.next()?.to_owned();
            let size = c.next()?.parse().unwrap_or(0);
            let installed = c.next()?.parse().ok();
            let description = c.next().unwrap_or("").to_owned();
            Some(NativePackage {
                name,
                version,
                description,
                size,
                install_date: installed,
                explicit: true,
            })
        })
        // Signing keys aren't software.
        .filter(|p| p.name != "gpg-pubkey")
        .filter(|p| seen.insert(p.name.clone()))
        .collect()
}

/// Package names from a `repoquery` listing (one per line; tolerant of the
/// odd status line DNF may print).
pub fn parse_name_list(text: &str) -> HashSet<String> {
    text.split(['\n', ' '])
        .flat_map(|w| w.split("\\n"))
        .map(str::trim)
        .filter(|w| crate::privileged::valid_package_name(w))
        .map(str::to_owned)
        .collect()
}

/// Package names from a Zypper table (`S | Repository | Name | Version | Arch`).
pub fn parse_zypper_table(text: &str) -> Vec<String> {
    let mut name_col = None;
    let mut names = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 3 {
            continue;
        }
        match name_col {
            None => name_col = cols.iter().position(|c| *c == "Name"),
            Some(i) => {
                if let Some(name) = cols
                    .get(i)
                    .filter(|n| crate::privileged::valid_package_name(n))
                {
                    names.push((*name).to_owned());
                }
            }
        }
    }
    names
}

fn dnf_auto_installed(installed: &[NativePackage]) -> Option<HashSet<String>> {
    let program = if program_exists("dnf") { "dnf" } else { "dnf5" };
    let user = read_command(
        program,
        &[
            "-C",
            "-q",
            "repoquery",
            "--userinstalled",
            "--queryformat",
            "%{name}\\n",
        ],
    )?;
    let user = parse_name_list(&user);
    if user.is_empty() {
        return None;
    }
    Some(
        installed
            .iter()
            .map(|p| p.name.clone())
            .filter(|n| !user.contains(n))
            .collect(),
    )
}

fn zypper_auto_installed() -> Option<HashSet<String>> {
    let text = std::fs::read_to_string("/var/lib/zypp/AutoInstalled").ok()?;
    Some(
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_owned)
            .collect(),
    )
}

/// Owners from `rpm -qf`, which prints one line per path, in order.
fn rpm_owners(paths: &[PathBuf]) -> HashMap<PathBuf, String> {
    let mut owners = HashMap::new();
    for chunk in paths.chunks(200) {
        let mut args: Vec<String> = vec![
            "-qf".into(),
            "--qf".into(),
            "%{NAME}\\n".into(),
            "--".into(),
        ];
        args.extend(chunk.iter().map(|p| p.display().to_string()));
        let Ok(out) = Command::new("rpm").args(&args).env("LC_ALL", "C").output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() != chunk.len() {
            continue;
        }
        for (path, line) in chunk.iter().zip(lines) {
            if !line.contains(' ') && !line.is_empty() {
                owners.insert(path.clone(), line.to_owned());
            }
        }
    }
    owners
}

#[cfg(test)]
mod tests {
    use super::*;

    const DPKG_STATUS_SAMPLE: &str = "\
Package: firefox
Status: install ok installed
Priority: optional
Installed-Size: 250000
Architecture: amd64
Version: 1:133.0+build1-0ubuntu1
Description: Safe and easy web browser from Mozilla
 Long description here.

Package: libc6
Status: install ok installed
Priority: required
Installed-Size: 13000
Architecture: amd64
Version: 2.39-0ubuntu8
Description: GNU C Library: Shared libraries

Package: libc6
Status: install ok installed
Priority: required
Installed-Size: 12000
Architecture: i386
Version: 2.39-0ubuntu8
Description: GNU C Library: Shared libraries

Package: removed-thing
Status: deinstall ok config-files
Installed-Size: 10
Architecture: amd64
Version: 1.0
Description: Gone but config left

Package: dpkg
Essential: yes
Status: install ok installed
Installed-Size: 6000
Architecture: amd64
Version: 1.22.6
Description: Debian package management system
";

    #[test]
    fn parses_dpkg_status() {
        let entries = parse_dpkg_status(DPKG_STATUS_SAMPLE);
        let names: Vec<&str> = entries.iter().map(|e| e.package.name.as_str()).collect();
        assert_eq!(names, ["firefox", "libc6", "libc6", "dpkg"]);
        let firefox = &entries[0].package;
        assert_eq!(firefox.size, 250_000 * 1024);
        assert_eq!(firefox.version, "1:133.0+build1-0ubuntu1");
        assert_eq!(
            firefox.description,
            "Safe and easy web browser from Mozilla"
        );
        assert!(!entries[0].essential);
        assert!(
            entries[1].essential,
            "Priority: required counts as essential"
        );
        assert!(entries[3].essential);
    }

    #[test]
    fn parses_extended_states() {
        let text = "Package: libfoo\nArchitecture: amd64\nAuto-Installed: 1\n\nPackage: vlc\nArchitecture: amd64\nAuto-Installed: 0\n";
        let auto = parse_apt_extended_states(text);
        assert!(auto.contains("libfoo"));
        assert!(!auto.contains("vlc"));
    }

    #[test]
    fn parses_apt_simulation() {
        let text = "\
NOTE: This is only a simulation!
Reading package lists...
The following packages will be REMOVED:
  libfoo1 linux-image-6.8.0-31-generic
Remv libfoo1 [1.2-3]
Remv linux-image-6.8.0-31-generic [6.8.0-31.31]
Remv libbar:i386 [2.0]
";
        assert_eq!(
            parse_apt_simulation(text),
            ["libfoo1", "linux-image-6.8.0-31-generic", "libbar"]
        );
    }

    #[test]
    fn parses_rpm_list() {
        let text = "firefox\t133.0-1.fc41\t250000000\t1730000000\tMozilla Firefox Web browser\n\
                    gpg-pubkey\tabc-def\t0\t1700000000\tgpg(Fedora)\n\
                    kernel-core\t6.11.4-301.fc41\t70000000\t1731000000\tThe Linux kernel\n\
                    broken line\n";
        let list = parse_rpm_list(text);
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["firefox", "kernel-core"]);
        assert_eq!(list[0].size, 250_000_000);
        assert_eq!(list[0].install_date, Some(1_730_000_000));
        assert_eq!(list[1].description, "The Linux kernel");
    }

    #[test]
    fn parses_name_lists_from_dnf() {
        let dnf4 = "libfoo\npython3-bar\n";
        let dnf5_literal = "libfoo\\npython3-bar\\n";
        let expected: HashSet<String> = ["libfoo".into(), "python3-bar".into()].into();
        assert_eq!(parse_name_list(dnf4), expected);
        assert_eq!(parse_name_list(dnf5_literal), expected);
    }

    #[test]
    fn parses_zypper_tables() {
        let text = "\
Loading repository data...
Reading installed packages...
S  | Repository | Name          | Version  | Arch
---+------------+---------------+----------+-------
i  | repo-oss   | libfoo1       | 1.2-1.1  | x86_64
i  | repo-oss   | python3-bar   | 2.0-2.3  | noarch
";
        assert_eq!(parse_zypper_table(text), ["libfoo1", "python3-bar"]);
    }

    #[test]
    fn remove_commands_never_ask() {
        let names = vec!["foo".to_owned()];
        let (p, a) = Backend::Pacman.remove_command(&names, true);
        assert_eq!(
            (p, a.join(" ")),
            ("/usr/bin/pacman", "-Rns --noconfirm -- foo".to_owned())
        );
        let (_, a) = Backend::Apt.remove_command(&names, true);
        assert_eq!(a.join(" "), "remove -y --autoremove -- foo");
        let (_, a) = Backend::Apt.remove_command(&names, false);
        assert_eq!(a.join(" "), "remove -y -- foo", "only the picked orphans");
        let (_, a) = Backend::Zypper.remove_command(&names, true);
        assert_eq!(a.join(" "), "--non-interactive remove --clean-deps -- foo");
        let (_, a) = Backend::Dnf.remove_command(&names, true);
        assert_eq!(a.join(" "), "remove -y -- foo");
    }

    #[test]
    fn explains_errors() {
        let pacman = ":: removing gtk3 breaks dependency 'gtk3' required by firefox\n";
        assert_eq!(explain_error(pacman), "Other software needs it: firefox");
        assert_eq!(
            explain_error("E: Unable to locate package nope\n"),
            "Unable to locate package nope"
        );
    }

    #[test]
    fn this_system_works() {
        let Some(backend) = Backend::detect() else {
            return;
        };
        let list = backend.list();
        assert!(!list.is_empty());
        assert!(backend.protected().contains("bash"));
        // Orphans are always a subset of what's installed.
        let orphans = backend.orphans(&list);
        assert!(
            orphans
                .iter()
                .all(|o| list.iter().any(|p| p.name == o.name))
        );
        match backend {
            Backend::Pacman => {
                // Matches the existing pacman logic.
                let db = packages::read_pacman_db(Path::new(PACMAN_DB));
                assert_eq!(orphans.len(), packages::pacman_orphans(&db).len());
                let owners = backend.owners_of(&[PathBuf::from("/usr/bin/pacman")]);
                assert_eq!(
                    owners.get(Path::new("/usr/bin/pacman")).map(String::as_str),
                    Some("pacman")
                );
            }
            Backend::Apt => {
                // On Debian and Ubuntu (and the CI runner) check real data.
                let owners = backend.owners_of(&[PathBuf::from("/usr/bin/apt-get")]);
                assert_eq!(
                    owners
                        .get(Path::new("/usr/bin/apt-get"))
                        .map(String::as_str),
                    Some("apt")
                );
                assert!(list.iter().any(|p| p.name == "dpkg" && p.size > 0));
                assert!(
                    list.iter().any(|p| !p.explicit),
                    "auto-installed packages are known"
                );
                assert!(
                    backend.protected().contains("dpkg"),
                    "essential packages are protected"
                );
                assert!(
                    backend.kernel_package().is_some(),
                    "the running kernel's package is found"
                );
                let preview = backend
                    .preview_removal(&["dpkg".to_owned()], &list)
                    .map(|p| p.complete);
                assert!(preview.is_err() || preview == Ok(true));
            }
            Backend::Dnf | Backend::Zypper => {}
        }
    }
}
