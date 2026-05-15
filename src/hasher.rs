use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rayon::prelude::*;

use crate::cache::{Cache, ChunkHash, FileEntry};

#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub blksz: u32,
}

impl Stat {
    pub fn from_metadata(m: &std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            ctime_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
            blksz: u32::try_from(m.blksize()).unwrap_or(u32::MAX),
        }
    }
}

// Chunk boundaries follow st_blksize so the hashes line up with ranges
// FICLONERANGE will accept. blake3 is plenty fast single-threaded per
// chunk; we get parallelism across files via rayon at the call site.
pub fn hash_file(path: &Path, blksz: u32) -> Result<Vec<ChunkHash>> {
    ensure!(blksz > 0, "blksz must be > 0");
    let mut f = File::open(path).with_context(|| format!("open {path:?}"))?;
    let mut buf = vec![0u8; blksz as usize];
    let mut hashes = Vec::new();
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        hashes.push(*blake3::hash(&buf[..n]).as_bytes());
        if n < buf.len() {
            break;
        }
    }
    Ok(hashes)
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut off = 0;
    while off < buf.len() {
        let n = f.read(&mut buf[off..])?;
        if n == 0 {
            break;
        }
        off += n;
    }
    Ok(off)
}

pub struct Hashed {
    pub stat: Stat,
    pub hashes: Vec<ChunkHash>,
    pub from_cache: bool,
}

pub fn hash_files(cache: &Cache, paths: &[PathBuf]) -> Vec<(PathBuf, Result<Hashed>)> {
    let results: Vec<_> = paths
        .par_iter()
        .map(|p| (p.clone(), hash_one(cache, p)))
        .collect();

    let fresh: Vec<_> = results
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok())
        .filter(|h| !h.from_cache)
        .map(|h| {
            (
                h.stat.dev,
                h.stat.ino,
                FileEntry {
                    size: h.stat.size,
                    mtime_ns: h.stat.mtime_ns,
                    ctime_ns: h.stat.ctime_ns,
                    blksz: h.stat.blksz,
                    hashes: h.hashes.clone(),
                },
            )
        })
        .collect();
    if !fresh.is_empty()
        && let Err(e) = cache.put_many(fresh.iter().map(|(d, i, e)| (*d, *i, e)))
    {
        eprintln!("warning: cache write failed: {e:#}");
    }
    results
}

fn hash_one(cache: &Cache, path: &Path) -> Result<Hashed> {
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("stat {path:?}"))?;
    ensure!(meta.is_file(), "not a regular file: {path:?}");
    let stat = Stat::from_metadata(&meta);

    if let Some(entry) = cache.get(stat.dev, stat.ino)?
        && entry.matches(stat.size, stat.mtime_ns, stat.ctime_ns, stat.blksz)
    {
        return Ok(Hashed {
            stat,
            hashes: entry.hashes,
            from_cache: true,
        });
    }

    Ok(Hashed {
        stat,
        hashes: hash_file(path, stat.blksz)?,
        from_cache: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn chunking() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"aaaabbbbcc").unwrap();
        let hs = hash_file(&p, 4).unwrap();
        assert_eq!(hs.len(), 3);
        assert_eq!(hs[0], *blake3::hash(b"aaaa").as_bytes());
        assert_eq!(hs[1], *blake3::hash(b"bbbb").as_bytes());
        assert_eq!(hs[2], *blake3::hash(b"cc").as_bytes());
    }

    #[test]
    fn empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e");
        File::create(&p).unwrap();
        assert!(hash_file(&p, 4096).unwrap().is_empty());
    }

    #[test]
    fn cache_hit_then_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, vec![7; 8192]).unwrap();
        let ps = std::slice::from_ref(&p);

        let a = hash_files(&cache, ps).pop().unwrap().1.unwrap();
        assert!(!a.from_cache);

        let b = hash_files(&cache, ps).pop().unwrap().1.unwrap();
        assert!(b.from_cache);
        assert_eq!(a.hashes, b.hashes);

        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(&[1; 4096])
            .unwrap();

        let c = hash_files(&cache, ps).pop().unwrap().1.unwrap();
        assert!(!c.from_cache);
        assert_ne!(b.hashes, c.hashes);
    }
}
