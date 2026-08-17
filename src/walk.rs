use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::sync_channel;

use anyhow::{Context, Result, bail};
use rustix::fs::{FsWord, statfs, statvfs};

// File paths prefix-compressed into a directory table. Every directory
// is (parent id, basename) and every file is (dir id, basename). All
// basenames live in one flat byte arena, so each path component is
// stored once rather than once per file.
pub struct Paths<T> {
    pub table: PathTable,
    pub files: Vec<(FilePath, T)>,
}

// `ids` is only needed for interning during the walk. seal() drops it.
#[derive(Default)]
pub struct PathTable {
    arena: Vec<u8>,
    dirs: Vec<Dir>,
    ids: HashMap<PathBuf, u32>,
}

struct Dir {
    parent: u32, // NO_PARENT for a root component
    off: u32,
    len: u32,
}

const NO_PARENT: u32 = u32::MAX;

impl PathTable {
    fn add_name(&mut self, name: &[u8]) -> (u32, u32) {
        let off = u32::try_from(self.arena.len()).expect("path arena overflow");
        self.arena.extend_from_slice(name);
        (off, name.len() as u32)
    }

    fn name(&self, off: u32, len: u32) -> &OsStr {
        OsStr::from_bytes(&self.arena[off as usize..(off + len) as usize])
    }

    fn intern(&mut self, p: &Path) -> u32 {
        if let Some(&id) = self.ids.get(p) {
            return id;
        }
        // "/" or "" become a root entry holding the whole path.
        let (parent, name) = match (p.parent(), p.file_name()) {
            (Some(par), Some(name)) => (self.intern(par), name),
            _ => (NO_PARENT, p.as_os_str()),
        };
        let (off, len) = self.add_name(name.as_bytes());
        let id = u32::try_from(self.dirs.len()).expect("dir table overflow");
        assert!(id != NO_PARENT, "dir table overflow");
        self.dirs.push(Dir { parent, off, len });
        self.ids.insert(p.to_path_buf(), id);
        id
    }

    fn dir_path(&self, mut id: u32) -> PathBuf {
        let mut parts = Vec::new();
        while id != NO_PARENT {
            let d = &self.dirs[id as usize];
            parts.push((d.off, d.len));
            id = d.parent;
        }
        let mut p = PathBuf::new();
        for &(off, len) in parts.iter().rev() {
            p.push(self.name(off, len));
        }
        p
    }

    fn seal(&mut self) {
        self.ids = HashMap::new();
    }
}

// 12 bytes.
pub struct FilePath {
    dir: u32,
    off: u32,
    len: u32,
}

impl FilePath {
    pub fn to_path(&self, table: &PathTable) -> PathBuf {
        table
            .dir_path(self.dir)
            .join(table.name(self.off, self.len))
    }
}

impl<T> Paths<T> {
    pub fn path(&self, fp: &FilePath) -> PathBuf {
        fp.to_path(&self.table)
    }

    fn push(&mut self, parent: &Path, name: &OsStr, payload: T) {
        let dir = self.table.intern(parent);
        let (off, len) = self.table.add_name(name.as_bytes());
        self.files.push((FilePath { dir, off, len }, payload));
    }

    // Re-tag the payloads in batches, sharing the same arena. The step
    // function controls when intermediate state (e.g. hash buffers) is
    // dropped between batches.
    pub fn map_batched<U>(
        self,
        batch: usize,
        mut step: impl FnMut(&PathTable, Vec<(FilePath, T)>) -> Result<Vec<(FilePath, U)>>,
    ) -> Result<Paths<U>> {
        let Self { table, files } = self;
        let mut out = Vec::with_capacity(files.len());
        let mut iter = files.into_iter();
        loop {
            let chunk: Vec<_> = iter.by_ref().take(batch).collect();
            if chunk.is_empty() {
                break;
            }
            out.extend(step(&table, chunk)?);
        }
        Ok(Paths { table, files: out })
    }

    // Drop payloads the closure rejects; the arena keeps unused names
    // (cheaper than compacting and harmless).
    pub fn filter_map<U>(
        self,
        mut f: impl FnMut(&FilePath, T, &PathTable) -> Option<U>,
    ) -> Paths<U> {
        let Self { table, files } = self;
        let files = files
            .into_iter()
            .filter_map(|(p, t)| f(&p, t, &table).map(|u| (p, u)))
            .collect();
        Paths { table, files }
    }
}

impl<T> Default for Paths<T> {
    fn default() -> Self {
        Self {
            table: PathTable::default(),
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
                p.parent().unwrap_or(Path::new("")),
                p.file_name().unwrap_or(OsStr::new("")),
                t,
            );
        }
        out.table.seal();
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
// Read from mountinfo, not the `mountpoint` property, so legacy mounts
// (fstab roots, docker's zfs graph driver) are found too. Subtree bind
// mounts (root != "/") are skipped; the dataset root already covers them.
pub fn zfs_mounts() -> Result<Vec<PathBuf>> {
    let pools = cloneable_pools()?;
    let mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").context("read /proc/self/mountinfo")?;
    Ok(zfs_mountpoints(&mountinfo, &pools))
}

fn zfs_mountpoints(mountinfo: &str, pools: &HashSet<String>) -> Vec<PathBuf> {
    mountinfo
        .lines()
        .filter_map(crate::remount::parse_mountinfo)
        .filter(|m| {
            m.fstype == "zfs"
                && m.root == Path::new("/")
                && pools.contains(m.source.split('/').next().unwrap_or(&m.source))
        })
        .map(|m| m.point)
        .collect()
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
    // All grew without a size hint; doubling leaves ~33% slack. The
    // realloc here returns it before hashing inflates RSS further.
    out.table.seal();
    out.files.shrink_to_fit();
    out.table.arena.shrink_to_fit();
    out.table.dirs.shrink_to_fit();
    out
}

// One directory's stat'ed files: (name, dev, ino, nlink) each.
type DirBatch = (PathBuf, Vec<(OsString, u64, u64, u64)>);

fn walk_dir<'a>(
    sc: &rayon::Scope<'a>,
    dir: PathBuf,
    root_dev: u64,
    tx: &'a std::sync::mpsc::SyncSender<DirBatch>,
) {
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("walk: {}: {e}", dir.display());
            return;
        }
    };
    let mut batch = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("walk: {}: {e}", dir.display());
                continue;
            }
        };
        // Does not follow symlinks.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                // Vanished mid-walk. Not an error.
                if e.kind() != ErrorKind::NotFound {
                    eprintln!("walk: {}: {e}", entry.path().display());
                }
                continue;
            }
        };
        if !ft.is_dir() && !ft.is_file() {
            continue;
        }
        let m = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    eprintln!("walk: {}: {e}", entry.path().display());
                }
                continue;
            }
        };
        if ft.is_dir() {
            // Prune at mount boundaries. Child datasets get their own
            // walk_root call from main.
            if m.dev() == root_dev {
                sc.spawn(move |sc| walk_dir(sc, entry.path(), root_dev, tx));
            }
        } else {
            batch.push((entry.file_name(), m.dev(), m.ino(), m.nlink()));
        }
    }
    if !batch.is_empty() {
        let _ = tx.send((dir, batch));
    }
}

fn walk_root(
    root: &Path,
    root_dev: u64,
    fsid: u64,
    exclude: &HashSet<(u64, u64)>,
    seen: &mut HashSet<(u64, u64)>,
    out: &mut Paths<u64>,
) {
    // Directories are read and files stat'ed in parallel on rayon
    // workers, which stream compact batches through a bounded channel
    // to the serial consumer below. A full channel blocks the workers,
    // so peak memory stays at O(workers * dir size + channel capacity)
    // no matter how far the walk runs ahead.
    let (tx, rx) = sync_channel::<DirBatch>(1024);
    std::thread::scope(|s| {
        s.spawn(|| {
            rayon::scope(|sc| walk_dir(sc, root.to_path_buf(), root_dev, &tx));
            drop(tx); // ends the rx loop
        });
        for (parent, files) in rx {
            for (name, dev, ino, nlink) in files {
                // A different dev means we crossed a mount point.
                if dev != root_dev || exclude.contains(&(dev, ino)) {
                    continue;
                }
                // `seen` collapses hardlinks. Files with nlink == 1
                // have no aliases and skip the HashSet.
                if nlink > 1 && !seen.insert((dev, ino)) {
                    continue;
                }
                out.push(&parent, &name, fsid);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_include_legacy_and_skip_binds_and_foreign() {
        let mountinfo = "\
39 1 0:33 / / rw,noatime shared:1 - zfs zroot/root/nixos rw,xattr,posixacl
54 39 0:33 /nix/store /nix/store ro,nosuid,nodev shared:17 - zfs zroot/root/nixos rw,xattr
60 39 0:40 / /var/lib/docker/zfs/graph/abc rw,relatime - zfs zroot/docker/abc rw,xattr
61 39 0:41 / /zroot rw,noatime - zfs zroot rw,xattr
62 39 0:50 / /old ro,relatime - zfs oldpool/data ro,xattr
63 39 259:1 / /boot rw,relatime - vfat /dev/nvme0n1p1 rw";
        let pools: HashSet<String> = ["zroot".to_owned()].into();
        let got = zfs_mountpoints(mountinfo, &pools);
        assert_eq!(
            got,
            vec![
                PathBuf::from("/"),
                PathBuf::from("/var/lib/docker/zfs/graph/abc"),
                PathBuf::from("/zroot"),
            ]
        );
    }

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
