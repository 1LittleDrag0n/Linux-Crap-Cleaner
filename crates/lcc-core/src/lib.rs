//! Core logic for Linux Crap Cleaner.
//!
//! Everything that scans, measures or cleans lives here, with no GUI code, so
//! it can be unit-tested and reused by the GUI, the privileged helper and a
//! future CLI.

pub mod desktop;
pub mod disks;
pub mod duplicates;
pub mod format;
pub mod junk;
pub mod leftovers;
pub mod packages;
pub mod privileged;
pub mod remove;
pub mod safety;
pub mod scan;
pub mod settings;
pub mod system;
pub mod usage;

/// Reverse-DNS application ID used for the desktop file, D-Bus name and
/// settings.
pub const APP_ID: &str = "io.github.LinuxCrapCleaner";

/// Human-readable application name.
pub const APP_NAME: &str = "Linux Crap Cleaner";

/// Directory name used under `~/.config` and `~/.cache`.
pub const APP_DIR_NAME: &str = "linux-crap-cleaner";
