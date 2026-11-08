//! Removing files and folders: the only place the app deletes anything.
//!
//! Every removal passes [`PathGuard::check`] first, never follows symlinks,
//! and either moves the item to the desktop Trash or deletes it, depending on
//! the user's setting.

use std::path::Path;

use crate::safety::{PathGuard, Protected};
use crate::settings::DeleteMode;

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error(transparent)]
    Protected(#[from] Protected),
    #[error("{0} no longer exists")]
    NotFound(String),
    #[error("could not delete {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("could not move {path} to the Trash: {message}")]
    Trash { path: String, message: String },
}

/// Removes `path` after checking it against `guard`.
///
/// A symlink is removed itself; its target is never touched.
pub fn remove(path: &Path, mode: DeleteMode, guard: &PathGuard) -> Result<(), RemoveError> {
    guard.check(path)?;

    let shown = path.display().to_string();
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RemoveError::NotFound(shown.clone())
        } else {
            RemoveError::Io {
                path: shown.clone(),
                source: e,
            }
        }
    })?;

    // Never reach into another drive mounted somewhere inside a folder.
    if meta.is_dir()
        && let Some(inner) = crate::disks::mounts_inside(&crate::disks::read_mounts(), path)
            .into_iter()
            .next()
    {
        return Err(RemoveError::Protected(Protected {
            path: path.to_path_buf(),
            reason: format!("another drive is mounted inside it, at {}", inner.display()),
        }));
    }

    match mode {
        DeleteMode::MoveToTrash => trash::delete(path).map_err(|e| RemoveError::Trash {
            path: shown,
            message: e.to_string(),
        }),
        DeleteMode::Permanent => {
            let result = if meta.is_dir() {
                // `remove_dir_all` does not follow symlinks inside the tree.
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            result.map_err(|source| RemoveError::Io {
                path: shown,
                source,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lcc-remove-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn guard() -> PathGuard {
        PathGuard::new(Path::new("/home/nobody-test"), &[])
    }

    #[test]
    fn deletes_files_and_folders_permanently() {
        let dir = temp("perm");
        fs::write(dir.join("f.txt"), "x").unwrap();
        fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        fs::write(dir.join("sub/deeper/g.txt"), "y").unwrap();

        remove(&dir.join("f.txt"), DeleteMode::Permanent, &guard()).unwrap();
        remove(&dir.join("sub"), DeleteMode::Permanent, &guard()).unwrap();
        assert!(!dir.join("f.txt").exists());
        assert!(!dir.join("sub").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removes_symlink_not_target() {
        let dir = temp("link");
        let target = dir.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "keep").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove(&link, DeleteMode::Permanent, &guard()).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(target.join("keep.txt").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_protected_paths() {
        let err = remove(Path::new("/usr/bin"), DeleteMode::Permanent, &guard()).unwrap_err();
        assert!(matches!(err, RemoveError::Protected(_)));
        assert!(Path::new("/usr/bin").exists());
    }

    #[test]
    fn missing_path_is_reported() {
        let dir = temp("missing");
        let err = remove(&dir.join("nope"), DeleteMode::Permanent, &guard()).unwrap_err();
        assert!(matches!(err, RemoveError::NotFound(_)));
        fs::remove_dir_all(dir).unwrap();
    }
}
