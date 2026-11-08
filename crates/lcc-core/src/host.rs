//! Running the system's own programs, and reading the system's environment.
//!
//! Inside the AppImage, GTK and GLib are pointed at the bundled copies through
//! environment variables (`GTK_PATH`, `GIO_MODULE_DIR`, `XDG_DATA_DIRS`, ...).
//! Programs from the system (flatpak, apt, systemctl, ...) must not see those,
//! or they would load the bundled modules into the system's libraries. The
//! AppImage's start-up hook (`packaging/appimage/apprun-hook.sh`) lists the
//! variables it changed in `LCC_APPIMAGE_ENV` and saves each original value
//! as `LCC_HOST_<name>`.
//!
//! Outside the AppImage these functions are plain `Command::new` and
//! `std::env::var_os`.

use std::ffi::{OsStr, OsString};
use std::process::Command;

/// `:`-separated names of the variables the AppImage changed.
const CHANGED: &str = "LCC_APPIMAGE_ENV";
/// Prefix of the saved original values.
const ORIGINAL: &str = "LCC_HOST_";

fn changed() -> Vec<String> {
    std::env::var(CHANGED)
        .map(|list| {
            list.split(':')
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A command for a program of the system, with the system's environment.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    for name in changed() {
        match std::env::var_os(format!("{ORIGINAL}{name}")) {
            Some(value) => cmd.env(&name, value),
            None => cmd.env_remove(&name),
        };
    }
    cmd
}

/// The system's value of an environment variable, as it was before the
/// AppImage changed it.
pub fn var_os(name: &str) -> Option<OsString> {
    if changed().iter().any(|n| n == name) {
        std::env::var_os(format!("{ORIGINAL}{name}"))
    } else {
        std::env::var_os(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_the_appimage_nothing_changes() {
        // The test runner isn't started by the AppImage hook.
        if std::env::var_os(CHANGED).is_some() {
            return;
        }
        assert_eq!(var_os("PATH"), std::env::var_os("PATH"));
        let cmd = command("true");
        assert_eq!(cmd.get_envs().count(), 0);
    }
}
