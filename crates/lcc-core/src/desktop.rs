//! App launchers (`.desktop` files): names and icons for the software list,
//! and detection of launchers whose program is gone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The parts of a `.desktop` file the app needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub path: PathBuf,
    /// File name without `.desktop`, e.g. `org.gnome.Nautilus`.
    pub id: String,
    pub name: String,
    pub comment: String,
    pub icon: Option<String>,
    pub exec: Option<String>,
    pub try_exec: Option<String>,
    /// Hidden from menus (`NoDisplay=true` or `Hidden=true`).
    pub hidden: bool,
    pub is_application: bool,
}

/// Parses the `[Desktop Entry]` group. Localized keys (`Name[de]=`) are
/// ignored in favour of the plain ones.
pub fn parse_desktop_entry(path: &Path, text: &str) -> DesktopEntry {
    let mut entry = DesktopEntry {
        path: path.to_path_buf(),
        id: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ..DesktopEntry::default()
    };
    let mut in_main = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_owned();
        match key.trim() {
            "Name" => entry.name = value,
            "Comment" => entry.comment = value,
            "Icon" => entry.icon = Some(value).filter(|v| !v.is_empty()),
            "Exec" => entry.exec = Some(value).filter(|v| !v.is_empty()),
            "TryExec" => entry.try_exec = Some(value).filter(|v| !v.is_empty()),
            "NoDisplay" | "Hidden" if value == "true" => entry.hidden = true,
            "Type" => entry.is_application = value == "Application",
            _ => {}
        }
    }
    entry
}

/// Folders holding launchers, user's first (user launchers override system
/// ones with the same id).
pub fn application_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = data_home {
        dirs.push(d.join("applications"));
        dirs.push(d.join("flatpak/exports/share/applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    for d in data_dirs.split(':').filter(|d| d.starts_with('/')) {
        dirs.push(Path::new(d).join("applications"));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// All launchers, keyed by id. The first folder wins for duplicate ids.
pub fn read_all() -> HashMap<String, DesktopEntry> {
    let mut all = HashMap::new();
    for dir in application_dirs() {
        for entry in read_dir_entries(&dir) {
            all.entry(entry.id.clone()).or_insert(entry);
        }
    }
    all
}

/// Launchers directly inside one folder.
pub fn read_dir_entries(dir: &Path) -> Vec<DesktopEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(parse_desktop_entry(&p, &text))
        })
        .collect()
}

/// The program a launcher runs: `TryExec`, or the first word of `Exec`,
/// skipping `env` and `VAR=value` prefixes.
pub fn launcher_program(entry: &DesktopEntry) -> Option<String> {
    if let Some(t) = &entry.try_exec {
        return Some(t.clone());
    }
    let words = split_exec(entry.exec.as_deref()?);
    let mut iter = words.into_iter().peekable();
    if iter
        .peek()
        .is_some_and(|w| w == "env" || w.ends_with("/env"))
    {
        iter.next();
        while iter
            .peek()
            .is_some_and(|w| w.contains('=') && !w.starts_with('/'))
        {
            iter.next();
        }
    }
    iter.next()
}

/// Splits an `Exec` value into words, honouring double quotes.
fn split_exec(exec: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            ' ' | '\t' if !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Whether `program` exists: an absolute path that exists, or a name found in
/// `PATH`.
pub fn program_exists(program: &str, path_var: &std::ffi::OsStr) -> bool {
    if program.starts_with('/') {
        return Path::new(program).exists();
    }
    if program.contains('/') {
        // Relative paths with folders can't be checked reliably; assume fine.
        return true;
    }
    std::env::split_paths(path_var).any(|d| d.join(program).is_file())
}

/// Whether the launcher's program is missing (the app was removed).
pub fn is_broken(entry: &DesktopEntry, path_var: &std::ffi::OsStr) -> bool {
    if !entry.is_application {
        return false;
    }
    match launcher_program(entry) {
        Some(program) => !program_exists(&program, path_var),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIREFOX: &str = "\
[Desktop Entry]
Version=1.0
Name=Firefox
Name[de]=Firefox-Browser
Comment=Browse the Web
Exec=/usr/lib/firefox/firefox %u
Icon=firefox
Type=Application

[Desktop Action new-window]
Name=New Window
Exec=/usr/lib/firefox/firefox --new-window %u
";

    #[test]
    fn parses_main_group_only() {
        let e = parse_desktop_entry(
            Path::new("/usr/share/applications/firefox.desktop"),
            FIREFOX,
        );
        assert_eq!(e.id, "firefox");
        assert_eq!(e.name, "Firefox");
        assert_eq!(e.comment, "Browse the Web");
        assert_eq!(e.icon.as_deref(), Some("firefox"));
        assert_eq!(e.exec.as_deref(), Some("/usr/lib/firefox/firefox %u"));
        assert!(e.is_application);
        assert!(!e.hidden);
    }

    #[test]
    fn finds_the_program() {
        let mut e = DesktopEntry {
            is_application: true,
            exec: Some("env GDK_BACKEND=x11 FOO=1 \"/opt/My App/app\" --flag %F".into()),
            ..Default::default()
        };
        assert_eq!(launcher_program(&e).as_deref(), Some("/opt/My App/app"));
        e.exec = Some("flatpak run org.gnome.Maps".into());
        assert_eq!(launcher_program(&e).as_deref(), Some("flatpak"));
        e.try_exec = Some("maps".into());
        assert_eq!(launcher_program(&e).as_deref(), Some("maps"));
    }

    #[test]
    fn detects_broken_launchers() {
        let path = std::ffi::OsString::from("/usr/bin:/bin");
        let mut e = DesktopEntry {
            is_application: true,
            exec: Some("sh -c true".into()),
            ..Default::default()
        };
        assert!(!is_broken(&e, &path));
        e.exec = Some("/nonexistent/app.AppImage %U".into());
        assert!(is_broken(&e, &path));
        e.exec = Some("definitely-not-installed-xyz".into());
        assert!(is_broken(&e, &path));
        e.is_application = false;
        assert!(!is_broken(&e, &path), "links and folders aren't checked");
    }
}
