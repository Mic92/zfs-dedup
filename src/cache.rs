use std::collections::HashSet;
use std::path::Path;

use anyhow::{Result, bail};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

pub const HASH_LEN: usize = 16;
pub type ChunkHash = [u8; HASH_LEN];

const FILES: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("files");

// Keyed on (dev, ino). Entry is stale if any of size/mtime/ctime/blksz
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
    pub hashes: Vec<ChunkHash>,
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

fn encode(e: EntryRef) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER + e.hashes.len() * HASH_LEN);
    buf.extend_from_slice(&e.size.to_le_bytes());
    buf.extend_from_slice(&e.mtime_ns.to_le_bytes());
    buf.extend_from_slice(&e.ctime_ns.to_le_bytes());
    buf.extend_from_slice(&e.blksz.to_le_bytes());
    for h in e.hashes {
        buf.extend_from_slice(h);
    }
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
}

impl Cache {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        tx.open_table(FILES)?;
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn get(&self, dev: u64, ino: u64) -> Result<Option<FileEntry>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(FILES)?;
        match table.get((dev, ino))? {
            Some(v) => Ok(Some(FileEntry::decode(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn put(&self, dev: u64, ino: u64, entry: EntryRef) -> Result<()> {
        self.put_many([(dev, ino, entry)])
    }

    pub fn put_many<'a>(
        &self,
        entries: impl IntoIterator<Item = (u64, u64, EntryRef<'a>)>,
    ) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILES)?;
            for (dev, ino, e) in entries {
                table.insert((dev, ino), encode(e).as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // Drop entries whose keys weren't seen this run. Keeps the DB from
    // growing forever as files get deleted or renamed across runs.
    pub fn prune(&self, seen: &HashSet<(u64, u64)>) -> Result<usize> {
        let tx = self.db.begin_write()?;
        let removed;
        {
            let mut table = tx.open_table(FILES)?;
            let stale: Vec<_> = table
                .iter()?
                .filter_map(|r| r.ok())
                .map(|(k, _)| k.value())
                .filter(|k| !seen.contains(k))
                .collect();
            removed = stale.len();
            for k in stale {
                table.remove(k)?;
            }
        }
        tx.commit()?;
        Ok(removed)
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
        let seen: HashSet<_> = [(0, 1), (0, 3)].into();
        assert_eq!(cache.prune(&seen).unwrap(), 3);
        assert!(cache.get(0, 0).unwrap().is_none());
        assert!(cache.get(0, 1).unwrap().is_some());
        assert!(cache.get(0, 2).unwrap().is_none());
        assert!(cache.get(0, 3).unwrap().is_some());
        assert!(cache.get(0, 4).unwrap().is_none());
    }
}
