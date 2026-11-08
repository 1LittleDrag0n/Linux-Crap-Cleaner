//! Snap packages (Ubuntu and others with snapd).
//!
//! Snapd keeps older revisions of every snap after updates ("disabled"),
//! which can add up to gigabytes on Ubuntu. Those, and whole snaps, are
//! removed by the administrator helper with `snap remove`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::desktop::{self, DesktopEntry};

pub const SNAPS_DIR: &str = "/var/lib/snapd/snaps";
pub const SNAP_DESKTOP_DIR: &str = "/var/lib/snapd/desktop/applications";

/// Snaps that keep the system or other snaps working.
pub const PROTECTED_SNAPS: &[&str] = &[
    "snapd", "core", "core16", "core18", "core20", "core22", "core24", "core26", "bare",
];

/// One line of `snap list --all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapEntry {
    pub name: String,
    pub version: String,
    pub revision: u32,
    pub notes: String,
}

impl SnapEntry {
    /// An old revision kept after an update.
    pub fn disabled(&self) -> bool {
        self.notes.split(',').any(|n| n == "disabled")
    }

    /// A base or the snapd snap rather than an app.
    pub fn is_system(&self) -> bool {
        self.notes
            .split(',')
            .any(|n| n == "base" || n == "core" || n == "snapd")
            || PROTECTED_SNAPS.contains(&self.name.as_str())
    }

    /// The downloaded snap file.
    pub fn file(&self) -> PathBuf {
        Path::new(SNAPS_DIR).join(format!("{}_{}.snap", self.name, self.revision))
    }

    pub fn size(&self) -> u64 {
        std::fs::metadata(self.file()).map(|m| m.len()).unwrap_or(0)
    }
}

/// Snap names: lowercase letters, digits and dashes.
pub fn valid_snap_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Parses `snap list --all`:
/// `Name  Version  Rev  Tracking  Publisher  Notes`.
pub fn parse_snap_list(text: &str) -> Vec<SnapEntry> {
    text.lines()
        .skip_while(|l| !l.starts_with("Name"))
        .skip(1)
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            if cols.len() < 6 {
                return None;
            }
            let name = cols[0];
            if !valid_snap_name(name) {
                return None;
            }
            Some(SnapEntry {
                name: name.to_owned(),
                version: cols[1].to_owned(),
                revision: cols[2].parse().ok()?,
                notes: cols[cols.len() - 1].to_owned(),
            })
        })
        .collect()
}

/// Every installed snap revision. Empty if snapd isn't installed.
pub fn list_all() -> Vec<SnapEntry> {
    let Ok(out) = Command::new("snap")
        .args(["list", "--all"])
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_snap_list(&String::from_utf8_lossy(&out.stdout))
}

/// Revisions kept after updates, which can be removed.
pub fn disabled_revisions() -> Vec<SnapEntry> {
    list_all().into_iter().filter(SnapEntry::disabled).collect()
}

/// Launchers of installed snaps, keyed by snap name (`firefox_firefox.desktop`
/// belongs to the `firefox` snap).
pub fn launchers() -> HashMap<String, DesktopEntry> {
    let mut map = HashMap::new();
    for e in desktop::read_dir_entries(Path::new(SNAP_DESKTOP_DIR)) {
        if e.hidden {
            continue;
        }
        if let Some((snap, _)) = e.id.split_once('_') {
            map.entry(snap.to_owned()).or_insert(e);
        }
    }
    map
}

/// `name=revision` arguments for the helper.
pub fn revision_arg(name: &str, revision: u32) -> String {
    format!("{name}={revision}")
}

/// Parses a `name=revision` argument.
pub fn parse_revision_arg(arg: &str) -> Option<(String, u32)> {
    let (name, rev) = arg.split_once('=')?;
    (valid_snap_name(name) && rev.len() <= 10 && rev.bytes().all(|b| b.is_ascii_digit()))
        .then(|| Some((name.to_owned(), rev.parse().ok()?)))
        .flatten()
}

/// Snap file name → (name, revision): `firefox_5134.snap`.
pub fn parse_snap_file(file_name: &str) -> Option<(String, u32)> {
    let stem = file_name.strip_suffix(".snap")?;
    let (name, rev) = stem.rsplit_once('_')?;
    (valid_snap_name(name))
        .then(|| Some((name.to_owned(), rev.parse().ok()?)))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Name               Version          Rev    Tracking         Publisher     Notes
bare               1.0              5      latest/stable    canonical✓    base
core22             20241001         1663   latest/stable    canonical✓    base
firefox            133.0-1          5134   latest/stable/…  mozilla✓      -
firefox            132.0.2-1        5091   latest/stable/…  mozilla✓      disabled
gnome-42-2204      0+git.510a601    176    latest/stable    canonical✓    disabled,base
snapd              2.66             23258  latest/stable    canonical✓    snapd
";

    #[test]
    fn parses_snap_list() {
        let list = parse_snap_list(SAMPLE);
        assert_eq!(list.len(), 6);
        let firefox_old = &list[3];
        assert_eq!(firefox_old.name, "firefox");
        assert_eq!(firefox_old.revision, 5091);
        assert!(firefox_old.disabled());
        assert!(!list[2].disabled());
        assert!(list[4].disabled() && list[4].is_system());
        assert!(list[5].is_system());
        assert!(!list[2].is_system());
        assert_eq!(
            list[2].file(),
            Path::new("/var/lib/snapd/snaps/firefox_5134.snap")
        );
    }

    #[test]
    fn names_and_arguments() {
        assert!(valid_snap_name("gnome-42-2204"));
        assert!(!valid_snap_name("Firefox"));
        assert!(!valid_snap_name("-rf"));
        assert!(!valid_snap_name("a/b"));
        assert_eq!(
            parse_revision_arg(&revision_arg("firefox", 5091)),
            Some(("firefox".into(), 5091))
        );
        assert_eq!(parse_revision_arg("firefox=x1"), None);
        assert_eq!(parse_revision_arg("../x=1"), None);
        assert_eq!(
            parse_snap_file("firefox_5091.snap"),
            Some(("firefox".into(), 5091))
        );
        assert_eq!(
            parse_snap_file("gnome-42-2204_176.snap"),
            Some(("gnome-42-2204".into(), 176))
        );
        assert_eq!(parse_snap_file("notes.txt"), None);
    }
}
