use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rayon::prelude::*;

use crate::cache::{Cache, ChunkHash, EntryRef, HASH_LEN};

// Sentinel for all-zero chunks. Cannot collide with a real XXH3-128
// output except at 2^-128, and the false positive would only produce a
// wasted byte-compare anyway. We skip these in the index: ZFS handles
// zero runs via compression (zle/lz4), and a sparse-file workload would
// otherwise put millions of locations in one bucket.
pub const ZERO_HASH: ChunkHash = [0u8; HASH_LEN];

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
// FICLONERANGE will accept. XXH3-128 is non-crypto; we always byte-verify
// candidate pairs before cloning, so the hash only has to keep the false-
// positive rate low. Cross-file parallelism comes from rayon at the call
// site.
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
        let chunk = &buf[..n];
        hashes.push(if is_zero(chunk) {
            ZERO_HASH
        } else {
            hash_chunk(chunk)
        });
        if n < buf.len() {
            break;
        }
    }
    Ok(hashes)
}

pub fn hash_chunk(buf: &[u8]) -> ChunkHash {
    xxhash_rust::xxh3::xxh3_128(buf).to_le_bytes()
}

// Slice equality compiles to memcmp, which is SIMD and faster than a
// hand-rolled u64 loop. For non-zero blocks the first cache line
// disagrees and memcmp bails immediately. For all-zero blocks the full
// scan still beats running xxhash.
fn is_zero(buf: &[u8]) -> bool {
    // Default ZFS recordsize; lives in .bss so it's free. Larger blocks
    // (recordsize up to 16M with feature@large_blocks) loop.
    static ZEROS: [u8; 1 << 17] = [0u8; 1 << 17];
    let mut rest = buf;
    while !rest.is_empty() {
        let n = rest.len().min(ZEROS.len());
        if rest[..n] != ZEROS[..n] {
            return false;
        }
        rest = &rest[n..];
    }
    true
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

pub fn hash_files(cache: &Cache, paths: &[PathBuf]) -> Result<Vec<(PathBuf, Result<Hashed>)>> {
    let results: Vec<_> = paths
        .par_iter()
        .map(|p| (p.clone(), hash_one(cache, p)))
        .collect();

    cache.put_many(
        results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|h| !h.from_cache)
            .map(|h| {
                (
                    h.stat.dev,
                    h.stat.ino,
                    EntryRef {
                        size: h.stat.size,
                        mtime_ns: h.stat.mtime_ns,
                        ctime_ns: h.stat.ctime_ns,
                        blksz: h.stat.blksz,
                        hashes: &h.hashes,
                    },
                )
            }),
    )?;
    Ok(results)
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
        assert_eq!(hs[0], hash_chunk(b"aaaa"));
        assert_eq!(hs[1], hash_chunk(b"bbbb"));
        assert_eq!(hs[2], hash_chunk(b"cc"));
    }

    #[test]
    fn zero_chunks_get_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("z");
        let mut data = vec![0u8; 4096];
        data.extend_from_slice(&[1u8; 4096]);
        data.extend_from_slice(&[0u8; 4096]);
        std::fs::write(&p, &data).unwrap();
        let hs = hash_file(&p, 4096).unwrap();
        assert_eq!(hs[0], ZERO_HASH);
        assert_ne!(hs[1], ZERO_HASH);
        assert_eq!(hs[2], ZERO_HASH);
        // Sentinel is reachable for truly-zero input only.
        assert!(is_zero(&[0; 4096]));
        assert!(!is_zero(&[0, 0, 0, 1]));
        assert!(!is_zero(&[1, 0, 0, 0]));
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

        let a = hash_files(&cache, ps).unwrap().pop().unwrap().1.unwrap();
        assert!(!a.from_cache);

        let b = hash_files(&cache, ps).unwrap().pop().unwrap().1.unwrap();
        assert!(b.from_cache);
        assert_eq!(a.hashes, b.hashes);

        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(&[1; 4096])
            .unwrap();

        let c = hash_files(&cache, ps).unwrap().pop().unwrap().1.unwrap();
        assert!(!c.from_cache);
        assert_ne!(b.hashes, c.hashes);
    }
}
