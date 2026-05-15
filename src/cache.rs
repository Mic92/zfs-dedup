//! Persistent per-file chunk-hash cache backed by redb.
//!
//! Keyed on `(dev, ino)`. A cached entry is valid only when size, mtime,
//! ctime, and blksz all match the current stat — otherwise the file was
//! modified (or the inode reused) and we must rehash.
//!
//! Hashes are stored in offset order: chunk `i` covers bytes
//! `[i * blksz, (i+1) * blksz)` (last chunk may be short).

use std::path::Path;

use anyhow::{Context, Result, bail};
use redb::{Database, ReadableDatabase, TableDefinition};

pub const HASH_LEN: usize = 32;
pub type ChunkHash = [u8; HASH_LEN];

/// `(dev, ino) -> serialized FileEntry`
const FILES: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("files");

/// Stat fingerprint plus the ordered chunk hashes for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub blksz: u32,
    pub hashes: Vec<ChunkHash>,
}

const HEADER_LEN: usize = 8 + 16 + 16 + 4;

impl FileEntry {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.hashes.len() * HASH_LEN);
        buf.extend_from_slice(&self.size.to_le_bytes());
        buf.extend_from_slice(&self.mtime_ns.to_le_bytes());
        buf.extend_from_slice(&self.ctime_ns.to_le_bytes());
        buf.extend_from_slice(&self.blksz.to_le_bytes());
        for h in &self.hashes {
            buf.extend_from_slice(h);
        }
        buf
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN || !(buf.len() - HEADER_LEN).is_multiple_of(HASH_LEN) {
            bail!("corrupt cache entry: length {}", buf.len());
        }
        let size = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let mtime_ns = i128::from_le_bytes(buf[8..24].try_into().unwrap());
        let ctime_ns = i128::from_le_bytes(buf[24..40].try_into().unwrap());
        let blksz = u32::from_le_bytes(buf[40..44].try_into().unwrap());
        let hashes = buf[HEADER_LEN..]
            .chunks_exact(HASH_LEN)
            .map(|c| c.try_into().unwrap())
            .collect();
        Ok(Self {
            size,
            mtime_ns,
            ctime_ns,
            blksz,
            hashes,
        })
    }

    /// Validity check against a freshly stat'd file.
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
        let db = Database::create(path).with_context(|| format!("open cache {path:?}"))?;
        // Ensure table exists so first read doesn't error.
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

    pub fn put(&self, dev: u64, ino: u64, entry: &FileEntry) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILES)?;
            table.insert((dev, ino), entry.encode().as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Batch insert in a single transaction — much faster than per-file `put`.
    pub fn put_many<'a>(
        &self,
        entries: impl IntoIterator<Item = (u64, u64, &'a FileEntry)>,
    ) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILES)?;
            for (dev, ino, e) in entries {
                table.insert((dev, ino), e.encode().as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: usize) -> FileEntry {
        FileEntry {
            size: 131072 * n as u64,
            mtime_ns: 1_700_000_000_000_000_000,
            ctime_ns: 1_700_000_000_000_000_001,
            blksz: 131072,
            hashes: (0..n).map(|i| [i as u8; 32]).collect(),
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let e = entry(3);
        cache.put(1, 42, &e).unwrap();
        assert_eq!(cache.get(1, 42).unwrap().unwrap(), e);
        assert!(cache.get(1, 43).unwrap().is_none());
    }

    #[test]
    fn encode_decode_empty() {
        let e = entry(0);
        assert_eq!(FileEntry::decode(&e.encode()).unwrap(), e);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(FileEntry::decode(&[0u8; 5]).is_err());
        assert!(FileEntry::decode(&[0u8; HEADER_LEN + 1]).is_err());
    }

    #[test]
    fn matches_fingerprint() {
        let e = entry(2);
        assert!(e.matches(e.size, e.mtime_ns, e.ctime_ns, e.blksz));
        assert!(!e.matches(e.size + 1, e.mtime_ns, e.ctime_ns, e.blksz));
        assert!(!e.matches(e.size, e.mtime_ns + 1, e.ctime_ns, e.blksz));
        assert!(!e.matches(e.size, e.mtime_ns, e.ctime_ns, e.blksz + 1));
    }

    #[test]
    fn put_many_batch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let es: Vec<_> = (0..10).map(entry).collect();
        cache
            .put_many(es.iter().enumerate().map(|(i, e)| (0u64, i as u64, e)))
            .unwrap();
        for (i, e) in es.iter().enumerate() {
            assert_eq!(cache.get(0, i as u64).unwrap().as_ref(), Some(e));
        }
    }
}
