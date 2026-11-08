//! What's inside a folder, for reviewing it before it's deleted.
//!
//! Names come from the disk, so every file is listed (the scan keeps only
//! the largest files per folder). Folder sizes come from the scan tree when
//! there is one, since measuring them again would mean walking the whole
//! folder.

use std::cmp::Reverse;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::scan::DirNode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Folder,
    File,
    /// A symbolic link: deleting it removes the link, not its target.
    Link,
    /// Sockets, pipes, device files.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: OsString,
    pub path: PathBuf,
    pub kind: ItemKind,
    /// `None` for a folder the scan didn't measure.
    pub size: Option<u64>,
    /// Files inside a folder, at any depth, when known.
    pub files: Option<u64>,
}

/// Lists `folder`, largest first. `node` is the folder's scan tree, if any.
pub fn list(folder: &Path, node: Option<&DirNode>) -> std::io::Result<Vec<Item>> {
    let mut items = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let ft = meta.file_type();
        let (kind, size, files) = if ft.is_symlink() {
            (ItemKind::Link, Some(0), None)
        } else if ft.is_dir() {
            let child = node.and_then(|n| n.child(&name));
            (
                ItemKind::Folder,
                child.map(|c| c.size),
                child.map(|c| c.file_count),
            )
        } else if ft.is_file() {
            (ItemKind::File, Some(meta.len()), None)
        } else {
            (ItemKind::Other, Some(0), None)
        };
        items.push(Item {
            name,
            path,
            kind,
            size,
            files,
        });
    }
    items.sort_by(|a, b| (Reverse(a.size), &a.name).cmp(&(Reverse(b.size), &b.name)));
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn lists_every_item_largest_first() {
        let dir = std::env::temp_dir().join(format!("lcc-contents-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("big.bin"), vec![0u8; 3000]).unwrap();
        fs::write(dir.join("small & odd <name>.txt"), "x").unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("link")).unwrap();

        let node = DirNode {
            children: vec![DirNode {
                name: "sub".into(),
                size: 5000,
                file_count: 7,
                ..Default::default()
            }],
            ..Default::default()
        };
        let items = list(&dir, Some(&node)).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        let names: Vec<_> = items.iter().map(|i| i.name.to_string_lossy()).collect();
        assert_eq!(names, ["sub", "big.bin", "small & odd <name>.txt", "link"]);
        assert_eq!(items[0].kind, ItemKind::Folder);
        assert_eq!(items[0].files, Some(7));
        assert_eq!(items[3].kind, ItemKind::Link);

        // Without a scan tree, folder sizes are unknown and listed last.
        let dir2 = std::env::temp_dir().join(format!("lcc-contents2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir2);
        fs::create_dir_all(dir2.join("sub")).unwrap();
        fs::write(dir2.join("a"), "x").unwrap();
        let items = list(&dir2, None).unwrap();
        fs::remove_dir_all(&dir2).unwrap();
        assert_eq!(items[0].name, "a");
        assert_eq!(items[1].size, None);
    }
}
