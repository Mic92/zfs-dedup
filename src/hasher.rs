use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use rustix::fs::OFlags;

use crate::cache::{Cache, ChunkHash, EntryRef, HASH_LEN};

// Sentinel for all-zero chunks. Collision with a real XXH3-128 output is
// 2^-128 and only costs a wasted memcmp. Skipped in the index: ZFS already
// collapses zero runs via compression, and sparse files would otherwise put
// millions of locations in one bucket.
pub const ZERO_HASH: ChunkHash = [0u8; HASH_LEN];

#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub fsid: u64, // dataset id, see walk::fsid
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub blksz: u32,
}

impl Stat {
    pub fn from_metadata(m: &std::fs::Metadata, fsid: u64) -> Self {
        Self {
            fsid,
            ino: m.ino(),
            size: m.size(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            ctime_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
            blksz: u32::try_from(m.blksize()).unwrap_or(u32::MAX),
        }
    }
}

// The walk only emits regular files, but a path can be swapped for a
// symlink or fifo before we open it. NOFOLLOW + NONBLOCK + an fstat on
// the fd close the stat-then-open window. Used by hasher and dedup.
pub const NOFOLLOW_NONBLOCK: i32 = OFlags::NOFOLLOW.union(OFlags::NONBLOCK).bits() as i32;

fn open_nofollow(path: &Path) -> std::io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(NOFOLLOW_NONBLOCK)
        .open(path)
}

// Chunk boundaries follow st_blksize so the hashes line up with ranges
// FICLONERANGE will accept. XXH3-128 is non-crypto; we always byte-verify
// candidate pairs before cloning, so the hash only has to keep the false-
// positive rate low. Cross-file parallelism comes from rayon at the call
// site.
pub fn hash_file(path: &Path, blksz: u32) -> Result<Vec<ChunkHash>> {
    let f = open_nofollow(path).with_context(|| format!("open {path:?}"))?;
    hash_fd(&f, blksz)
}

fn hash_fd(mut f: impl Read, blksz: u32) -> Result<Vec<ChunkHash>> {
    ensure!(blksz > 0, "blksz must be > 0");
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

// Slice eq compiles to memcmp: SIMD, bails on first differing cache line
// for non-zero blocks, and beats xxhash for full all-zero scans.
fn is_zero(buf: &[u8]) -> bool {
    // Default ZFS recordsize, .bss so it's free; larger blocks loop.
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

fn read_full(f: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
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

pub fn hash_files(
    cache: &Cache,
    paths: &[(PathBuf, u64)],
) -> Result<Vec<(PathBuf, Result<Hashed>)>> {
    let results: Vec<_> = paths
        .par_iter()
        .map(|(p, fsid)| (p.clone(), hash_one(cache, p, *fsid)))
        .collect();

    cache.put_many(
        results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|h| !h.from_cache)
            .map(|h| {
                (
                    h.stat.fsid,
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

fn hash_one(cache: &Cache, path: &Path, fsid: u64) -> Result<Hashed> {
    let f = open_nofollow(path).with_context(|| format!("open {path:?}"))?;
    let meta = f.metadata()?;
    ensure!(meta.is_file(), "not a regular file: {path:?}");
    let stat = Stat::from_metadata(&meta, fsid);

    if let Some(entry) = cache.get(stat.fsid, stat.ino)?
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
        hashes: hash_fd(&f, stat.blksz)?,
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
    fn zero_sentinel() {
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
    fn rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        std::fs::write(&target, b"data").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(hash_file(&link, 4096).is_err(), "followed symlink");
    }

    #[test]
    fn empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e");
        File::create(&p).unwrap();
        assert!(hash_file(&p, 4096).unwrap().is_empty());
    }

    #[test]
    fn invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, vec![7; 8192]).unwrap();
        let ps = [(p.clone(), 0u64)];

        let a = hash_files(&cache, &ps).unwrap().pop().unwrap().1.unwrap();
        assert!(!a.from_cache);

        let b = hash_files(&cache, &ps).unwrap().pop().unwrap().1.unwrap();
        assert!(b.from_cache);
        assert_eq!(a.hashes, b.hashes);

        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(&[1; 4096])
            .unwrap();

        let c = hash_files(&cache, &ps).unwrap().pop().unwrap().1.unwrap();
        assert!(!c.from_cache);
        assert_ne!(b.hashes, c.hashes);
    }
}
