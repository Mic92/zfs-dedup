use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use redb::{Database, ReadableDatabase, TableDefinition};

pub const HASH_LEN: usize = 16;
pub type ChunkHash = [u8; HASH_LEN];

const FILES: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("files");

// Keyed on (fsid, ino); see walk::fsid for why not st_dev.
// Entry is stale if any of size/mtime/ctime/blksz
// changed. Hashes are in offset order, chunk i covers [i*blksz, (i+1)*blksz).
//
// Note: ZFS reports st_blksize == the file's actual on-disk blocksize, which
// for files smaller than recordsize is the file size rounded up. Files only
// reach the dataset recordsize once they grow past one record. zfs_clone_range
// rejects cross-blocksize clones, so the dedup stage must group by blksz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub blksz: u32,
    pub hashes: Box<[ChunkHash]>,
}

// Borrowed view for encoding without cloning the hash vector.
#[derive(Clone, Copy)]
pub struct EntryRef<'a> {
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub blksz: u32,
    pub hashes: &'a [ChunkHash],
}

const HEADER: usize = 8 + 16 + 16 + 4;

fn encoded_len(e: EntryRef) -> usize {
    HEADER + e.hashes.len() * HASH_LEN
}

fn encode_into(buf: &mut [u8], e: EntryRef) {
    debug_assert_eq!(buf.len(), encoded_len(e));
    buf[0..8].copy_from_slice(&e.size.to_le_bytes());
    buf[8..24].copy_from_slice(&e.mtime_ns.to_le_bytes());
    buf[24..40].copy_from_slice(&e.ctime_ns.to_le_bytes());
    buf[40..44].copy_from_slice(&e.blksz.to_le_bytes());
    for (dst, h) in buf[HEADER..].chunks_exact_mut(HASH_LEN).zip(e.hashes) {
        dst.copy_from_slice(h);
    }
}

#[cfg(test)]
fn encode(e: EntryRef) -> Vec<u8> {
    let mut buf = vec![0u8; encoded_len(e)];
    encode_into(&mut buf, e);
    buf
}

impl FileEntry {
    fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER || !(buf.len() - HEADER).is_multiple_of(HASH_LEN) {
            bail!("corrupt cache entry, len {}", buf.len());
        }
        // Slices are exact-length after the bounds check above; try_into
        // can't fail but the compiler doesn't know.
        fn arr<const N: usize>(b: &[u8]) -> [u8; N] {
            b.try_into().expect("len checked")
        }
        Ok(Self {
            size: u64::from_le_bytes(arr(&buf[0..8])),
            mtime_ns: i128::from_le_bytes(arr(&buf[8..24])),
            ctime_ns: i128::from_le_bytes(arr(&buf[24..40])),
            blksz: u32::from_le_bytes(arr(&buf[40..44])),
            hashes: buf[HEADER..].chunks_exact(HASH_LEN).map(arr).collect(),
        })
    }

    pub fn as_ref(&self) -> EntryRef<'_> {
        EntryRef {
            size: self.size,
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
            blksz: self.blksz,
            hashes: &self.hashes,
        }
    }

    pub fn matches(&self, size: u64, mtime_ns: i128, ctime_ns: i128, blksz: u32) -> bool {
        self.size == size
            && self.mtime_ns == mtime_ns
            && self.ctime_ns == ctime_ns
            && self.blksz == blksz
    }
}

pub struct Cache {
    db: Database,
    path: PathBuf,
}

impl Cache {
    pub fn open(path: &Path) -> Result<Self> {
        // redb's default page cache is 1 GiB; for our mostly-sequential
        // access it just double-caches the OS page cache and dominated
        // peak RSS in profiling. The OS page cache already keeps the
        // file warm.
        let db = Database::builder().set_cache_size(64 << 20).create(path)?;
        let tx = db.begin_write()?;
        tx.open_table(FILES)?;
        tx.commit()?;
        Ok(Self {
            db,
            path: path.to_owned(),
        })
    }

    pub fn get(&self, fsid: u64, ino: u64) -> Result<Option<FileEntry>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(FILES)?;
        match table.get((fsid, ino))? {
            Some(v) => FileEntry::decode(v.value())
                .with_context(|| format!("delete {} to rebuild the cache", self.path.display()))
                .map(Some),
            None => Ok(None),
        }
    }

    pub fn put(&self, fsid: u64, ino: u64, entry: EntryRef) -> Result<()> {
        self.put_many([(fsid, ino, entry)])
    }

    // insert_reserve writes directly into the redb page; encode() into a
    // Vec first would alloc + memcpy ~1 KiB per file.
    pub fn put_many<'a>(
        &self,
        entries: impl IntoIterator<Item = (u64, u64, EntryRef<'a>)>,
    ) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILES)?;
            for (fsid, ino, e) in entries {
                let mut g = table.insert_reserve((fsid, ino), encoded_len(e))?;
                encode_into(g.as_mut(), e);
            }
        }
        tx.commit()?;
        Ok(())
    }

    // Drop unseen entries on datasets we scanned. Scoping to `fsids`
    // keeps a partial scan from evicting cache for trees it never
    // visited. `seen` may over-answer (Bloom filter): a false positive
    // keeps a stale entry one run too long.
    pub fn prune(&self, seen: impl Fn(u64, u64) -> bool, fsids: &HashSet<u64>) -> Result<usize> {
        let tx = self.db.begin_write()?;
        let mut removed = 0;
        {
            let mut table = tx.open_table(FILES)?;
            table.retain(|(fsid, ino), _| {
                let keep = !fsids.contains(&fsid) || seen(fsid, ino);
                if !keep {
                    removed += 1;
                }
                keep
            })?;
        }
        tx.commit()?;
        Ok(removed)
    }

    // Reclaim file space when more than half the file is freed pages.
    // redb reuses them internally but never shrinks the file, so a big
    // prune leaves it at peak size. Compaction is slow; the floor keeps
    // small caches from rewriting for a few wasted MiB.
    pub fn compact_if_bloated(&mut self) -> Result<bool> {
        const FLOOR: u64 = 16 << 20;
        let file = std::fs::metadata(&self.path)?.len();
        // stored + metadata = live data; allocated_pages also counts
        // the free list and overstates use right after a prune.
        let used = {
            let tx = self.db.begin_write()?;
            let s = tx.stats()?;
            tx.abort()?;
            s.stored_bytes() + s.metadata_bytes()
        };
        if file > used.saturating_mul(2) && file.saturating_sub(used) > FLOOR {
            return Ok(self.db.compact()?);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: usize) -> FileEntry {
        FileEntry {
            size: 131072 * n as u64,
            mtime_ns: 1,
            ctime_ns: 2,
            blksz: 131072,
            hashes: (0..n).map(|i| [i as u8; HASH_LEN]).collect(),
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let e = entry(3);
        cache.put(1, 42, e.as_ref()).unwrap();
        assert_eq!(cache.get(1, 42).unwrap().unwrap(), e);
        assert!(cache.get(1, 43).unwrap().is_none());
    }

    #[test]
    fn encode_empty() {
        let e = entry(0);
        assert_eq!(FileEntry::decode(&encode(e.as_ref())).unwrap(), e);
    }

    #[test]
    fn decode_garbage() {
        assert!(FileEntry::decode(&[0; 5]).is_err());
        assert!(FileEntry::decode(&[0; HEADER + 1]).is_err());
    }

    #[test]
    fn matches() {
        let e = entry(2);
        assert!(e.matches(e.size, e.mtime_ns, e.ctime_ns, e.blksz));
        assert!(!e.matches(e.size + 1, e.mtime_ns, e.ctime_ns, e.blksz));
        assert!(!e.matches(e.size, e.mtime_ns + 1, e.ctime_ns, e.blksz));
        assert!(!e.matches(e.size, e.mtime_ns, e.ctime_ns, e.blksz + 1));
    }

    #[test]
    fn batch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let es: Vec<_> = (0..10).map(entry).collect();
        cache
            .put_many(
                es.iter()
                    .enumerate()
                    .map(|(i, e)| (0, i as u64, e.as_ref())),
            )
            .unwrap();
        for (i, e) in es.iter().enumerate() {
            assert_eq!(cache.get(0, i as u64).unwrap().as_ref(), Some(e));
        }
    }

    #[test]
    fn prune_unseen() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let e = entry(1);
        for i in 0..5 {
            cache.put(0, i, e.as_ref()).unwrap();
        }
        let seen: HashSet<_> = [(0u64, 1u64), (0, 3)].into();
        assert_eq!(
            cache
                .prune(|f, i| seen.contains(&(f, i)), &[0].into())
                .unwrap(),
            3
        );
        assert!(cache.get(0, 0).unwrap().is_none());
        assert!(cache.get(0, 1).unwrap().is_some());
        assert!(cache.get(0, 2).unwrap().is_none());
        assert!(cache.get(0, 3).unwrap().is_some());
        assert!(cache.get(0, 4).unwrap().is_none());
    }

    #[test]
    fn compact_if_bloated() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        assert!(!cache.compact_if_bloated().unwrap(), "fresh cache");
        // Grow past FLOOR, prune all, expect compaction once.
        let e = entry(1024); // ~16 KiB
        cache
            .put_many((0..2000).map(|i| (0, i, e.as_ref())))
            .unwrap();
        cache.prune(|_, _| false, &[0].into()).unwrap();
        assert!(cache.compact_if_bloated().unwrap(), "after big prune");
        assert!(!cache.compact_if_bloated().unwrap(), "already compacted");
    }

    #[test]
    fn prune_scoped_to_scanned_fsids() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let e = entry(1);
        cache.put(1, 0, e.as_ref()).unwrap();
        cache.put(2, 0, e.as_ref()).unwrap();
        // Scanned fsid 1, saw nothing: prune fsid 1 only, fsid 2 untouched.
        assert_eq!(cache.prune(|_, _| false, &[1].into()).unwrap(), 1);
        assert!(cache.get(1, 0).unwrap().is_none());
        assert!(cache.get(2, 0).unwrap().is_some());
    }
}
