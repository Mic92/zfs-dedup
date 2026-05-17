use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustix::fs::{FsWord, statfs, statvfs};

// A path split at the last component. The parent Arc is shared between
// siblings (jwalk already keeps it that way), so for trees with many
// files per directory the per-file footprint is the filename plus a
// pointer instead of the full path string. Cuts the path list -- the
// largest fixed cost on big scans -- by ~3x.
pub struct FilePath {
    parent: Arc<Path>,
    name: Box<OsStr>,
}

impl FilePath {
    pub fn to_path(&self) -> PathBuf {
        self.parent.join(Path::new(&self.name))
    }

    // For tests and fixtures that already have a full PathBuf.
    pub fn from_path(p: &Path) -> Self {
        Self {
            parent: Arc::from(p.parent().unwrap_or(Path::new(""))),
            name: Box::from(p.file_name().unwrap_or(OsStr::new(""))),
        }
    }
}

impl fmt::Debug for FilePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_path().fmt(f)
    }
}

impl fmt::Display for FilePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_path().display().fmt(f)
    }
}

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

fn zfs_cmd(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("run `{cmd}` (is ZFS installed and in PATH?)"))?;
    if !out.status.success() {
        bail!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// Pools with block_cloning enabled. Without it, FICLONERANGE and
// FIDEDUPERANGE return EOPNOTSUPP and the dataset can't be deduped.
fn cloneable_pools() -> Result<HashSet<String>> {
    let out = zfs_cmd(
        "zpool",
        &["get", "-H", "-o", "name,value", "feature@block_cloning"],
    )
    .context("query block_cloning pool feature (requires OpenZFS 2.2+)")?;
    let mut pools = HashSet::new();
    for (p, v) in out.lines().filter_map(|l| l.split_once('\t')) {
        if matches!(v, "active" | "enabled") {
            pools.insert(p.to_owned());
        } else {
            eprintln!("skip pool {p}: block_cloning feature not enabled");
        }
    }
    Ok(pools)
}

// Mountpoints of every mounted ZFS dataset on a block_cloning pool.
pub fn zfs_mounts() -> Result<Vec<PathBuf>> {
    let pools = cloneable_pools()?;
    let out = zfs_cmd(
        "zfs",
        &["list", "-H", "-t", "filesystem", "-o", "name,mountpoint"],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(ds, mp)| {
            mp.starts_with('/') && pools.contains(ds.split('/').next().unwrap_or(ds))
        })
        .map(|(_, mp)| PathBuf::from(mp))
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
) -> Vec<(FilePath, u64)> {
    let mut seen = HashSet::new();
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
        walk_root(root, dev, fsid, exclude, &mut seen, &mut out);
    }
    out
}

// (dev, ino, nlink) carried out of the parallel jwalk callback so the
// serial consumer doesn't re-stat every file.
type FileMeta = Option<(u64, u64, u64)>;

fn walk_root(
    root: &Path,
    root_dev: u64,
    fsid: u64,
    exclude: &HashSet<(u64, u64)>,
    seen: &mut HashSet<(u64, u64)>,
    out: &mut Vec<(FilePath, u64)>,
) {
    // process_read_dir runs on rayon threads: stat there so the per-file
    // syscall is parallel. The for loop below is single-threaded and
    // must only touch what the callback already collected.
    for entry in jwalk::WalkDirGeneric::<((), FileMeta)>::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .process_read_dir(move |_, _, _, children| {
            for c in children.iter_mut().flatten() {
                let ft = c.file_type();
                if !ft.is_dir() && !ft.is_file() {
                    continue;
                }
                let m = match c.metadata() {
                    Ok(m) => m,
                    // Files vanish mid-walk; not an error. On other
                    // errors leave client_state None: the consumer skips
                    // the file, but a dir is still descended -- pruning
                    // silently would drop the subtree.
                    Err(e) => {
                        if e.io_error()
                            .is_none_or(|io| io.kind() != ErrorKind::NotFound)
                        {
                            eprintln!("walk: {}: {e}", c.path().display());
                        }
                        continue;
                    }
                };
                if ft.is_dir() {
                    // Prune at mount boundaries; child datasets get
                    // their own walk_root call from main.
                    if m.dev() != root_dev {
                        c.read_children_path = None;
                    }
                } else {
                    c.client_state = Some((m.dev(), m.ino(), m.nlink()));
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
        let Some((dev, ino, nlink)) = entry.client_state else {
            continue;
        };
        // Stay on the root filesystem; a different dev means we crossed
        // a mount point, which could be non-ZFS or a different pool.
        if dev != root_dev || exclude.contains(&(dev, ino)) {
            continue;
        }
        // `seen` exists to dedup hardlinks. Files with nlink == 1 have
        // no aliases, so don't pay HashSet memory for them; that's most
        // of any tree.
        if nlink > 1 && !seen.insert((dev, ino)) {
            continue;
        }
        out.push((
            FilePath {
                parent: entry.parent_path.clone(),
                name: entry.file_name.clone().into_boxed_os_str(),
            },
            fsid,
        ));
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
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            super::walk_root(p, dev, 0, excl, &mut seen, &mut out);
            out.into_iter()
                .map(|(f, _)| f.name.to_string_lossy().into_owned())
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
