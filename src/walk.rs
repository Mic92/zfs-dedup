use std::collections::HashSet;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use rustix::fs::{FsWord, statfs, statvfs};

// Not in rustix's exported constants yet.
const ZFS_SUPER_MAGIC: FsWord = 0x2fc1_2fc1;

// We only know how to clone safely on ZFS: variable st_blksize chunking,
// recordsize alignment, no FIDEDUPERANGE. Other reflink FSes (btrfs/xfs)
// would technically work but with wrong assumptions; refuse them for now.
pub fn is_zfs(p: &Path) -> bool {
    statfs(p)
        .map(|s| s.f_type == ZFS_SUPER_MAGIC)
        .unwrap_or(false)
}

// Mountpoints of every mounted ZFS dataset.
pub fn zfs_mounts() -> Result<Vec<PathBuf>> {
    let out = Command::new("zfs")
        .args(["list", "-H", "-t", "filesystem", "-o", "mountpoint"])
        .output()
        .context("run `zfs list` (is ZFS installed and in PATH?)")?;
    if !out.status.success() {
        bail!("zfs list failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with('/'))
        .map(PathBuf::from)
        // Datasets can have a mountpoint set but not be mounted.
        .filter(|p| is_zfs(p))
        .collect())
}

// Per-dataset filesystem id from statvfs. ZFS derives this from the
// dataset's persistent fsid_guid, so unlike st_dev it is stable across
// reboots, remounts, and pool import order.
pub fn fsid(p: &Path) -> Result<u64> {
    Ok(statvfs(p)?.f_fsid)
}

// Collect regular files tagged with their dataset fsid. Hardlinked sets
// are collapsed to one path: same inode means already-shared storage, and
// zfs_clone_range would just hit the same dnode. Symlinks not followed.
// `exclude` pre-seeds the seen set, e.g. to skip our own cache file.
pub fn files<'a>(
    roots: impl IntoIterator<Item = &'a PathBuf>,
    exclude: &HashSet<(u64, u64)>,
) -> Vec<(PathBuf, u64)> {
    let mut seen = exclude.clone();
    let mut out = Vec::new();
    for root in roots {
        let stat = || anyhow::Ok((std::fs::metadata(root)?.dev(), fsid(root)?));
        let (dev, fsid) = match stat() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("walk: {}: {e}", root.display());
                continue;
            }
        };
        walk_root(root, dev, fsid, &mut seen, &mut out);
    }
    out
}

fn walk_root(
    root: &Path,
    root_dev: u64,
    fsid: u64,
    seen: &mut HashSet<(u64, u64)>,
    out: &mut Vec<(PathBuf, u64)>,
) {
    // Prune the walk at mount boundaries; child datasets get their own
    // walk_root call from main.
    for entry in jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .process_read_dir(move |_, _, _, children| {
            for c in children.iter_mut().flatten() {
                if c.file_type().is_dir() && c.metadata().map_or(true, |m| m.dev() != root_dev) {
                    c.read_children_path = None;
                }
            }
        })
    {
        let entry = match entry {
            Ok(e) => e,
            // Files vanish during a live walk all the time; not an error.
            Err(e)
                if e.io_error()
                    .is_some_and(|io| io.kind() == ErrorKind::NotFound) =>
            {
                continue;
            }
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
            Err(e)
                if e.io_error()
                    .is_some_and(|io| io.kind() == ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => {
                eprintln!("walk: {}: {e}", entry.path().display());
                continue;
            }
        };
        // Stay on the root filesystem; a different dev means we crossed
        // a mount point, which could be non-ZFS or a different pool.
        if meta.dev() == root_dev && seen.insert((meta.dev(), meta.ino())) {
            out.push((entry.path(), fsid));
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

        let dev = std::fs::metadata(p).unwrap().dev();
        let names = |excl: &HashSet<(u64, u64)>| {
            let mut seen = excl.clone();
            let mut out = Vec::new();
            super::walk_root(p, dev, 0, &mut seen, &mut out);
            out.into_iter()
                .map(|(f, _)| f.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        let got = names(&HashSet::new());
        // a/a2 collapse to one (whichever jwalk hits first), plus b and sub/c.
        assert_eq!(got.len(), 3);
        assert!(got.contains(&"b".into()));
        assert!(got.contains(&"c".into()));
        assert!(got.contains(&"a".into()) || got.contains(&"a2".into()));

        // Exclude b by (dev, ino).
        let bm = std::fs::metadata(p.join("b")).unwrap();
        let got = names(&[(bm.dev(), bm.ino())].into());
        assert_eq!(got.len(), 2);
        assert!(!got.contains(&"b".into()));
    }
}
