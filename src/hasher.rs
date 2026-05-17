use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::fs::File;
use std::io::Read;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::walk::Paths;
use std::ptr::NonNull;

use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use rustix::fs::{OFlags, fcntl_setfl};

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

    pub fn entry<'a>(&self, hashes: &'a [ChunkHash]) -> EntryRef<'a> {
        EntryRef {
            size: self.size,
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
            blksz: self.blksz,
            hashes,
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

// Page-aligned heap buffer; required for O_DIRECT reads.
struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len.max(1), 4096).expect("valid layout");
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })
            .unwrap_or_else(|| handle_alloc_error(layout));
        Self { ptr, layout }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

// Best-effort O_DIRECT: bypass the ARC so a cold scan over TBs doesn't
// evict the system's hot data. ZFS 2.3+ honours it (direct=standard
// default); older ZFS and other fses ignore or refuse it, which we
// ignore. Reads must be page-multiple, hence the blksz guard.
fn try_o_direct(f: &File, blksz: u32) {
    if blksz.is_multiple_of(4096) {
        let _ = fcntl_setfl(f, OFlags::DIRECT | OFlags::NONBLOCK);
    }
}

// Chunk boundaries follow st_blksize so the hashes line up with ranges
// FICLONERANGE will accept. XXH3-128 is non-crypto; we always byte-verify
// candidate pairs before cloning, so the hash only has to keep the false-
// positive rate low. Cross-file parallelism comes from rayon at the call
// site.
pub fn hash_file(path: &Path, blksz: u32) -> Result<Vec<ChunkHash>> {
    let f = open_nofollow(path).with_context(|| format!("open {path:?}"))?;
    try_o_direct(&f, blksz);
    hash_fd(&f, blksz)
}

fn hash_fd(mut f: impl Read, blksz: u32) -> Result<Vec<ChunkHash>> {
    ensure!(blksz > 0, "blksz must be > 0");
    let mut buf = AlignedBuf::new(blksz as usize);
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

// No hashes here: they live in the cache only. Holding them per-file
// dominated RSS on big trees, and the index is the only consumer.
pub struct Hashed {
    pub stat: Stat,
    pub from_cache: bool,
}

// Hash and persist `paths`; the returned set carries no hashes.
// Process in batches so the per-file hash vectors are short-lived
// instead of accumulating until the end.
pub fn hash_files(cache: &Cache, paths: Paths<u64>) -> Result<Paths<Result<Hashed>>> {
    paths.map_batched(100_000, |arena, batch| {
        let results: Vec<_> = batch
            .into_par_iter()
            .map(|(p, fsid)| {
                let r = hash_one(cache, &p.to_path(arena), fsid);
                (p, r)
            })
            .collect();
        cache.put_many(results.iter().filter_map(|(_, r)| match r {
            Ok((h, hashes)) if !h.from_cache => {
                Some((h.stat.fsid, h.stat.ino, h.stat.entry(hashes)))
            }
            _ => None,
        }))?;
        Ok(results
            .into_iter()
            .map(|(p, r)| (p, r.map(|(h, _)| h)))
            .collect())
    })
}

fn hash_one(cache: &Cache, path: &Path, fsid: u64) -> Result<(Hashed, Box<[ChunkHash]>)> {
    let f = open_nofollow(path).with_context(|| format!("open {path:?}"))?;
    let meta = f.metadata()?;
    ensure!(meta.is_file(), "not a regular file: {path:?}");
    let stat = Stat::from_metadata(&meta, fsid);

    if let Some(entry) = cache.get(stat.fsid, stat.ino)?
        && entry.matches(stat.size, stat.mtime_ns, stat.ctime_ns, stat.blksz)
    {
        return Ok((
            Hashed {
                stat,
                from_cache: true,
            },
            entry.hashes,
        ));
    }

    try_o_direct(&f, stat.blksz);
    Ok((
        Hashed {
            stat,
            from_cache: false,
        },
        hash_fd(&f, stat.blksz)?.into_boxed_slice(),
    ))
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
    fn aligned_buf() {
        for n in [1, 4096, 4097, 131072] {
            let mut b = AlignedBuf::new(n);
            assert_eq!(b.len(), n);
            assert_eq!(b.as_ptr() as usize % 4096, 0, "misaligned at n={n}");
            b.fill(0xab);
            assert!(b.iter().all(|&x| x == 0xab));
        }
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
        let ps = || [(p.clone(), 0u64)].into_iter().collect::<Paths<u64>>();
        let one = |paths: Paths<u64>| {
            hash_files(&cache, paths)
                .unwrap()
                .files
                .pop()
                .unwrap()
                .1
                .unwrap()
        };
        let cached = |s: &Stat| cache.get(s.fsid, s.ino).unwrap().unwrap().hashes;

        let a = one(ps());
        assert!(!a.from_cache);
        let ha = cached(&a.stat);

        let b = one(ps());
        assert!(b.from_cache);
        assert_eq!(ha, cached(&b.stat));

        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(&[1; 4096])
            .unwrap();

        let c = one(ps());
        assert!(!c.from_cache);
        assert_ne!(ha, cached(&c.stat));
    }
}
