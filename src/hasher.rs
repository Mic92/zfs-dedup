//! Chunk hashing at filesystem-blocksize granularity.
//!
//! ZFS clones operate on recordsize-aligned ranges, so chunk boundaries
//! must match `st_blksize` of the file. blake3 is fast enough that one
//! thread per chunk saturates NVMe; cross-file parallelism comes from
//! rayon at the call site.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use rayon::prelude::*;

use crate::cache::{Cache, ChunkHash, FileEntry};

/// Stat fingerprint we key the cache on.
#[derive(Debug, Clone, Copy)]
pub struct Fingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub blksz: u32,
}

impl Fingerprint {
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

/// Hash all blksz-aligned chunks of a file. Sequential read, single pass.
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
            break; // short read = EOF
        }
    }
    Ok(hashes)
}

/// Read until `buf` is full or EOF. Returns bytes read.
fn read_full(f: &mut File, buf: &mut [u8]) -> Result<usize> {
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

/// Result of hashing one file (cache hit or fresh).
pub struct HashedFile {
    pub fp: Fingerprint,
    pub hashes: Vec<ChunkHash>,
    pub cache_hit: bool,
}

pub type Hashed = Vec<(std::path::PathBuf, HashedFile)>;
pub type Failed = Vec<(std::path::PathBuf, anyhow::Error)>;

/// Hash a set of files in parallel, consulting and updating the cache.
///
/// Files that fail to stat/read are skipped (returned in `errors`).
pub fn hash_files(cache: &Cache, paths: &[std::path::PathBuf]) -> (Hashed, Failed) {
    let results: Vec<_> = paths
        .par_iter()
        .map(|p| (p.clone(), hash_one(cache, p)))
        .collect();

    let mut ok = Vec::new();
    let mut errors = Vec::new();
    let mut to_store = Vec::new();
    for (p, r) in results {
        match r {
            Ok(hf) => {
                if !hf.cache_hit {
                    to_store.push((
                        hf.fp.dev,
                        hf.fp.ino,
                        FileEntry {
                            size: hf.fp.size,
                            mtime_ns: hf.fp.mtime_ns,
                            ctime_ns: hf.fp.ctime_ns,
                            blksz: hf.fp.blksz,
                            hashes: hf.hashes.clone(),
                        },
                    ));
                }
                ok.push((p, hf));
            }
            Err(e) => errors.push((p, e)),
        }
    }
    if !to_store.is_empty()
        && let Err(e) = cache.put_many(to_store.iter().map(|(d, i, e)| (*d, *i, e)))
    {
        errors.push((std::path::PathBuf::from("<cache>"), e));
    }
    (ok, errors)
}

fn hash_one(cache: &Cache, path: &Path) -> Result<HashedFile> {
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("stat {path:?}"))?;
    ensure!(meta.is_file(), "not a regular file: {path:?}");
    let fp = Fingerprint::from_metadata(&meta);

    if let Some(entry) = cache.get(fp.dev, fp.ino)?
        && entry.matches(fp.size, fp.mtime_ns, fp.ctime_ns, fp.blksz)
    {
        return Ok(HashedFile {
            fp,
            hashes: entry.hashes,
            cache_hit: true,
        });
    }

    let hashes = hash_file(path, fp.blksz)?;
    Ok(HashedFile {
        fp,
        hashes,
        cache_hit: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hash_file_chunks_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        // 2.5 chunks at blksz=4
        std::fs::write(&p, b"aaaabbbbcc").unwrap();
        let hs = hash_file(&p, 4).unwrap();
        assert_eq!(hs.len(), 3);
        assert_eq!(hs[0], *blake3::hash(b"aaaa").as_bytes());
        assert_eq!(hs[1], *blake3::hash(b"bbbb").as_bytes());
        assert_eq!(hs[2], *blake3::hash(b"cc").as_bytes());
    }

    #[test]
    fn hash_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e");
        std::fs::File::create(&p).unwrap();
        assert!(hash_file(&p, 4096).unwrap().is_empty());
    }

    #[test]
    fn cache_hit_after_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, vec![7u8; 8192]).unwrap();

        let (ok1, err1) = hash_files(&cache, std::slice::from_ref(&p));
        assert!(err1.is_empty());
        assert!(!ok1[0].1.cache_hit);

        let (ok2, err2) = hash_files(&cache, std::slice::from_ref(&p));
        assert!(err2.is_empty());
        assert!(ok2[0].1.cache_hit);
        assert_eq!(ok1[0].1.hashes, ok2[0].1.hashes);

        // Modify file -> cache miss.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[1u8; 4096]).unwrap();
        drop(f);
        let (ok3, _) = hash_files(&cache, &[p]);
        assert!(!ok3[0].1.cache_hit);
        assert_ne!(ok2[0].1.hashes, ok3[0].1.hashes);
    }
}
