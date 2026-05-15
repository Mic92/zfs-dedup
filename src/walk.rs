use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FsWord, statfs};

// Not in rustix's exported constants yet.
const ZFS_SUPER_MAGIC: FsWord = 0x2fc1_2fc1;

// We only know how to clone safely on ZFS: variable st_blksize chunking,
// recordsize alignment, no FIDEDUPERANGE. Other reflink FSes (btrfs/xfs)
// would technically work but with wrong assumptions; refuse them for now.
fn is_zfs(p: &Path) -> bool {
    statfs(p)
        .map(|s| s.f_type == ZFS_SUPER_MAGIC)
        .unwrap_or(false)
}

// Collect regular files. Hardlinked sets are collapsed to one path: same
// inode means already-shared storage, and zfs_clone_range would just hit
// the same dnode. Symlinks not followed.
pub fn files(roots: &[PathBuf], require_zfs: bool) -> Vec<PathBuf> {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        if require_zfs && !is_zfs(root) {
            eprintln!(
                "skip {}: not a ZFS filesystem (use --force to override)",
                root.display()
            );
            continue;
        }
        let dev = match std::fs::metadata(root) {
            Ok(m) => m.dev(),
            Err(e) => {
                eprintln!("walk: {}: {e}", root.display());
                continue;
            }
        };
        walk_root(root, dev, &mut seen, &mut out);
    }
    out
}

fn walk_root(root: &Path, root_dev: u64, seen: &mut HashSet<(u64, u64)>, out: &mut Vec<PathBuf>) {
    for entry in jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("walk: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("walk: {}: {e}", entry.path().display());
                continue;
            }
        };
        // Stay on the root filesystem; a different dev means we crossed
        // a mount point, which could be non-ZFS or a different pool.
        if meta.dev() == root_dev && seen.insert((meta.dev(), meta.ino())) {
            out.push(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("a"), b"x").unwrap();
        std::fs::write(p.join("b"), b"y").unwrap();
        std::fs::hard_link(p.join("a"), p.join("a2")).unwrap();
        std::fs::create_dir(p.join("sub")).unwrap();
        std::fs::write(p.join("sub/c"), b"z").unwrap();
        std::os::unix::fs::symlink(p.join("a"), p.join("alink")).unwrap();

        let mut got: Vec<_> = files(std::slice::from_ref(&p.to_path_buf()), false)
            .into_iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        // a/a2 collapse to one (whichever jwalk hits first), plus b and sub/c.
        assert_eq!(got.len(), 3);
        assert!(got.contains(&"b".into()));
        assert!(got.contains(&"c".into()));
        assert!(got.contains(&"a".into()) || got.contains(&"a2".into()));
    }
}
