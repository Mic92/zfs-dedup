use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustix::fs::{FsWord, statfs, statvfs};

// File paths split at the last component. Parents are Arc-shared with
// siblings (jwalk already keeps it that way) and filenames live in one
// flat byte arena: no per-file heap allocation, no fat-pointer slop.
// On a 30M-file scan this cuts the path list -- the largest fixed cost
// in RAM -- by more than half.
pub struct Paths<T> {
    arena: Vec<u8>,
    pub files: Vec<(FilePath, T)>,
}

// 24 bytes; was 32 plus a heap allocation per file.
pub struct FilePath {
    parent: Arc<Path>,
    off: u32,
    len: u32,
}

impl FilePath {
    pub fn to_path(&self, arena: &[u8]) -> PathBuf {
        let name = &arena[self.off as usize..(self.off + self.len) as usize];
        self.parent.join(Path::new(OsStr::from_bytes(name)))
    }
}

impl<T> Paths<T> {
    pub fn path(&self, fp: &FilePath) -> PathBuf {
        fp.to_path(&self.arena)
    }

    fn push(&mut self, parent: Arc<Path>, name: &OsStr, payload: T) {
        let off = u32::try_from(self.arena.len()).expect("path arena overflow");
        self.arena.extend_from_slice(name.as_bytes());
        self.files.push((
            FilePath {
                parent,
                off,
                len: name.len() as u32,
            },
            payload,
        ));
    }

    // Re-tag the payloads in batches, sharing the same arena. The step
    // function controls when intermediate state (e.g. hash buffers) is
    // dropped between batches.
    pub fn map_batched<U>(
        self,
        batch: usize,
        mut step: impl FnMut(&[u8], Vec<(FilePath, T)>) -> Result<Vec<(FilePath, U)>>,
    ) -> Result<Paths<U>> {
        let Self { arena, files } = self;
        let mut out = Vec::with_capacity(files.len());
        let mut iter = files.into_iter();
        loop {
            let chunk: Vec<_> = iter.by_ref().take(batch).collect();
            if chunk.is_empty() {
                break;
            }
            out.extend(step(&arena, chunk)?);
        }
        Ok(Paths { arena, files: out })
    }

    // Drop payloads the closure rejects; the arena keeps unused names
    // (cheaper than compacting and harmless).
    pub fn filter_map<U>(self, mut f: impl FnMut(&FilePath, T, &[u8]) -> Option<U>) -> Paths<U> {
        let Self { arena, files } = self;
        let files = files
            .into_iter()
            .filter_map(|(p, t)| f(&p, t, &arena).map(|u| (p, u)))
            .collect();
        Paths { arena, files }
    }
}

impl<T> Default for Paths<T> {
    fn default() -> Self {
        Self {
            arena: Vec::new(),
            files: Vec::new(),
        }
    }
}

// For tests and fixtures that already have full PathBufs.
impl<T> FromIterator<(PathBuf, T)> for Paths<T> {
    fn from_iter<I: IntoIterator<Item = (PathBuf, T)>>(iter: I) -> Self {
        let mut out = Self::default();
        for (p, t) in iter {
            out.push(
                Arc::from(p.parent().unwrap_or(Path::new(""))),
                p.file_name().unwrap_or(OsStr::new("")),
                t,
            );
        }
        out
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
) -> Paths<u64> {
    let mut seen = HashSet::new();
    let mut out = Paths::default();
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
    // Both grew without a size hint; doubling leaves ~33% slack. The
    // realloc here returns it before hashing inflates RSS further.
    out.files.shrink_to_fit();
    out.arena.shrink_to_fit();
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
    out: &mut Paths<u64>,
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
        out.push(entry.parent_path.clone(), &entry.file_name, fsid);
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
            let mut out = Paths::default();
            super::walk_root(p, dev, 0, excl, &mut seen, &mut out);
            out.files
                .iter()
                .map(|(f, _)| {
                    out.path(f)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
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
