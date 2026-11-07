//! User settings, stored as TOML in `~/.config/linux-crap-cleaner/settings.toml`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What happens to files the user cleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteMode {
    /// Move to the desktop Trash so the user can restore them. Caches are
    /// still deleted directly, since trashing them would not free space.
    #[default]
    MoveToTrash,
    /// Delete immediately. Cannot be undone.
    Permanent,
}

/// Light or dark appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub delete_mode: DeleteMode,
    /// How many versions of each package to keep in the package cache.
    pub keep_package_versions: u32,
    /// Do not cross into other mounted disks while scanning.
    pub stay_on_one_filesystem: bool,
    /// Include hidden files (dot files) in the Storage Analyzer.
    pub show_hidden_files: bool,
    /// Opt-in background tracking of which apps run, for the usage screen.
    pub track_app_usage: bool,
    /// Folders the app must never scan or clean.
    pub excluded_paths: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            delete_mode: DeleteMode::MoveToTrash,
            keep_package_versions: 2,
            stay_on_one_filesystem: true,
            show_hidden_files: true,
            track_app_usage: false,
            excluded_paths: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("could not find the config folder")]
    NoConfigDir,
    #[error("could not read or write {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("settings file {path} is invalid: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not save settings: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl Settings {
    /// Default location of the settings file.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join(crate::APP_DIR_NAME).join("settings.toml"))
    }

    /// Loads settings from the default location. A missing file gives the
    /// defaults; a broken one is logged and also gives the defaults, so the
    /// app always starts.
    pub fn load() -> Self {
        let Some(path) = Self::default_path() else {
            tracing::warn!("no config folder; using default settings");
            return Self::default();
        };
        match Self::load_from(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("{e}; using default settings");
                Self::default()
            }
        }
    }

    /// Loads settings from `path`. A missing file gives the defaults.
    pub fn load_from(path: &Path) -> Result<Self, SettingsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(SettingsError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    /// Saves to the default location.
    pub fn save(&self) -> Result<(), SettingsError> {
        let path = Self::default_path().ok_or(SettingsError::NoConfigDir)?;
        self.save_to(&path)
    }

    /// Saves to `path`, writing a temporary file first so a crash never
    /// leaves a half-written settings file.
    pub fn save_to(&self, path: &Path) -> Result<(), SettingsError> {
        let io_err = |source| SettingsError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(io_err)?;
        std::fs::rename(&tmp, path).map_err(io_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("lcc-settings-{}-{name}", std::process::id()))
            .join("settings.toml")
    }

    #[test]
    fn missing_file_gives_defaults() {
        let path = temp_file("missing");
        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
    }

    #[test]
    fn round_trip() {
        let path = temp_file("roundtrip");
        let settings = Settings {
            theme: Theme::Dark,
            delete_mode: DeleteMode::Permanent,
            keep_package_versions: 1,
            stay_on_one_filesystem: false,
            show_hidden_files: false,
            track_app_usage: true,
            excluded_paths: vec![PathBuf::from("/home/test/keep")],
        };
        settings.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn unknown_and_missing_keys_are_tolerated() {
        let parsed: Settings = toml::from_str("theme = \"dark\"\nfuture_option = 3\n").unwrap();
        assert_eq!(parsed.theme, Theme::Dark);
        assert_eq!(parsed.delete_mode, DeleteMode::MoveToTrash);
    }

    #[test]
    fn broken_file_is_an_error() {
        let path = temp_file("broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "theme = [").unwrap();
        let result = Settings::load_from(&path);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert!(matches!(result, Err(SettingsError::Parse { .. })));
    }
}
