use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::cache::ChunkHash;
use crate::clone::{Dedupe, clone_range, dedupe_range};
use crate::hasher::{Hashed, NOFOLLOW_NONBLOCK, ZERO_HASH};

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub candidates: usize,
    pub verified: usize,
    pub cloned: usize,
    pub bytes: u64,
    pub mismatches: usize,
    pub errors: usize,
}

impl std::ops::AddAssign for Stats {
    fn add_assign(&mut self, o: Stats) {
        self.candidates += o.candidates;
        self.verified += o.verified;
        self.cloned += o.cloned;
        self.bytes += o.bytes;
        self.mismatches += o.mismatches;
        self.errors += o.errors;
    }
}

#[derive(Clone, Copy)]
pub struct Loc {
    file: usize,
    chunk: u64,
}

// XXH3-128 keys are already uniform; SipHash on top is wasted CPU. Take
// the last 8 bytes of the chunk hash and fold in blksz.
#[derive(Default)]
pub struct ChunkKeyHasher(u64);

impl Hasher for ChunkKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        // Called once for the [u8; 16] field. Use its tail.
        if bytes.len() >= 8 {
            self.0 ^= u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().expect("len >= 8"));
        } else {
            for &b in bytes {
                self.0 = self.0.rotate_left(8) ^ b as u64;
            }
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.0 ^= u64::from(n).wrapping_mul(0x9e3779b97f4a7c15);
    }
}

pub type Index = HashMap<(u32, ChunkHash), Vec<Loc>, BuildHasherDefault<ChunkKeyHasher>>;

// Truncated key for the seen/dup pre-filter; halves its footprint.
// A collision only marks a singleton as a possible dup; pass 2 indexes
// by full key so the false group has one location and is a no-op.
fn dup_key(blksz: u32, h: &ChunkHash) -> u64 {
    u64::from_le_bytes(h[8..16].try_into().expect("16-byte hash"))
        ^ u64::from(blksz).wrapping_mul(0x9e3779b97f4a7c15)
}

// One candidate group per (blksz, hash). zfs_clone_range refuses
// cross-blocksize clones, so files with different blksz never share.
//
// Two passes: mark which (blksz, hash) keys repeat, then collect
// locations only for those. Building a Vec per chunk and discarding
// the singletons would transiently allocate several times the final
// index size on low-dup trees.
pub fn build_index(files: &[(PathBuf, Hashed)]) -> Index {
    type Pre = HashSet<u64, BuildHasherDefault<ChunkKeyHasher>>;
    let mut seen = Pre::default();
    let mut dup = Pre::default();
    for (_, h) in files {
        for hash in &h.hashes {
            if *hash == ZERO_HASH {
                continue;
            }
            let k = dup_key(h.stat.blksz, hash);
            if !seen.insert(k) {
                dup.insert(k);
            }
        }
    }
    drop(seen);

    let mut idx = Index::default();
    for (fi, (_, h)) in files.iter().enumerate() {
        for (ci, hash) in h.hashes.iter().enumerate() {
            if *hash == ZERO_HASH || !dup.contains(&dup_key(h.stat.blksz, hash)) {
                continue;
            }
            idx.entry((h.stat.blksz, *hash)).or_default().push(Loc {
                file: fi,
                chunk: ci as u64,
            });
        }
    }
    // Truncated-key collisions in `dup` make singletons here; drop them
    // so group() doesn't open+read a source with nothing to clone to.
    idx.retain(|_, v| v.len() > 1);
    idx
}

// Per-rayon-task state. Read buffers reused across candidates; no fd
// cache because n_workers * n_files would blow ulimit -n on big trees,
// and open() is dwarfed by the verify reads anyway. Two tasks may open
// the same file (independent fds) and clone into different offsets
// concurrently, which ZFS handles fine. Groups never share a
// (file, chunk) since each chunk has exactly one hash.
struct Worker<'a> {
    files: &'a [(PathBuf, Hashed)],
    opts: Opts,
    buf_a: Vec<u8>,
    buf_b: Vec<u8>,
    stats: Stats,
}

#[derive(Clone, Copy)]
pub struct Opts {
    pub dry_run: bool,
    pub fideduperange: bool,
}

impl<'a> Worker<'a> {
    fn new(files: &'a [(PathBuf, Hashed)], opts: Opts) -> Self {
        Self {
            files,
            opts,
            buf_a: Vec::new(),
            buf_b: Vec::new(),
            stats: Stats::default(),
        }
    }

    fn open(&self, i: usize, write: bool) -> Result<File> {
        File::options()
            .read(true)
            .write(write && !self.opts.dry_run)
            .custom_flags(NOFOLLOW_NONBLOCK)
            .open(&self.files[i].0)
            .with_context(|| format!("open {:?}", self.files[i].0))
    }

    fn group(&mut self, blksz: u64, locs: &[Loc]) {
        // locs[0] is the canonical source: open and read it once,
        // clone the rest from it.
        let src = locs[0];
        let len = chunk_len(&self.files[src.file].1, src.chunk, blksz);
        if len == 0 {
            return; // past stat.size: file grew after stat
        }
        let src_off = src.chunk * blksz;
        let compare = self.opts.dry_run || !self.opts.fideduperange;

        // Source is read-only: dedup must work on files we can't modify.
        let prep = |w: &mut Self| -> Result<File> {
            let sf = w.open(src.file, false)?;
            if compare {
                w.buf_a.resize(len as usize, 0);
                sf.read_exact_at(&mut w.buf_a, src_off)?;
            }
            Ok(sf)
        };
        let sf = match prep(self) {
            Ok(f) => f,
            Err(e) if is_not_found(&e) => return,
            Err(e) => {
                eprintln!("skip group {:?}+{src_off}: {e:#}", self.files[src.file].0);
                self.stats.errors += 1;
                return;
            }
        };

        for &dst in &locs[1..] {
            if len != chunk_len(&self.files[dst.file].1, dst.chunk, blksz) {
                continue; // tail-vs-full mismatch or stale index
            }
            self.stats.candidates += 1;
            match self.verify_and_clone(&sf, src_off, dst, blksz, len, compare) {
                Ok(Some(bytes)) => {
                    self.stats.verified += 1;
                    if !self.opts.dry_run {
                        self.stats.cloned += 1;
                    }
                    self.stats.bytes += bytes;
                }
                Ok(None) => self.stats.mismatches += 1,
                // File vanished or shrank since we hashed it.
                Err(e) if is_not_found(&e) => {}
                Err(e) => {
                    eprintln!(
                        "skip {:?}+{} <- {:?}+{src_off}: {e:#}",
                        self.files[dst.file].0,
                        dst.chunk * blksz,
                        self.files[src.file].0,
                    );
                    self.stats.errors += 1;
                }
            }
        }
    }

    // Some(bytes deduped) on match, None on mismatch. `compare` selects
    // userspace verify+FICLONERANGE; otherwise FIDEDUPERANGE in-kernel.
    fn verify_and_clone(
        &mut self,
        sf: &File,
        src_off: u64,
        dst: Loc,
        blksz: u64,
        len: u64,
        compare: bool,
    ) -> Result<Option<u64>> {
        let dst_off = dst.chunk * blksz;
        let df = self.open(dst.file, true)?;

        if !compare {
            return match dedupe_range(sf, src_off, &df, dst_off, len).context("FIDEDUPERANGE")? {
                Dedupe::Same(b) => Ok(Some(b)),
                Dedupe::Differs => Ok(None),
                Dedupe::Unsupported => anyhow::bail!("FIDEDUPERANGE unsupported"),
            };
        }

        // Source chunk is in buf_a from group(). Racy against concurrent
        // writers, hence --force.
        self.buf_b.resize(len as usize, 0);
        df.read_exact_at(&mut self.buf_b, dst_off)?;
        if self.buf_a != self.buf_b {
            return Ok(None);
        }
        if !self.opts.dry_run {
            clone_range(sf, src_off, &df, dst_off, len).context("FICLONERANGE")?;
        }
        Ok(Some(len))
    }
}

pub fn dedup(files: &[(PathBuf, Hashed)], opts: Opts) -> Stats {
    let idx = build_index(files);
    let groups: Vec<_> = idx.into_iter().collect();
    groups
        .into_par_iter()
        .fold(
            || Worker::new(files, opts),
            |mut w, ((blksz, _), locs)| {
                w.group(blksz as u64, &locs);
                w
            },
        )
        .map(|w| w.stats)
        .reduce(Stats::default, |mut a, b| {
            a += b;
            a
        })
}

// anyhow's downcast_ref only checks the outermost error; walk the chain.
pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}

// 0 if `chunk` is past stat.size: file grew between stat() and the
// hash read, leaving stale entries in the index. Callers skip those.
fn chunk_len(h: &Hashed, chunk: u64, blksz: u64) -> u64 {
    let off = chunk * blksz;
    h.stat.size.saturating_sub(off).min(blksz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::{Stat, hash_chunk, hash_file};

    const DRY: Opts = Opts {
        dry_run: true,
        fideduperange: false,
    };

    // Build a (path, Hashed) pair with a fixed blksz, ignoring whatever
    // st_blksize the test filesystem happens to report.
    fn fixture(dir: &std::path::Path, name: &str, data: &[u8], blksz: u32) -> (PathBuf, Hashed) {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        let m = std::fs::metadata(&p).unwrap();
        let mut stat = Stat::from_metadata(&m, 0);
        stat.blksz = blksz;
        let hashes = hash_file(&p, blksz).unwrap();
        (
            p,
            Hashed {
                stat,
                hashes,
                from_cache: false,
            },
        )
    }

    #[test]
    fn not_found_chain() {
        let io = std::io::Error::from(std::io::ErrorKind::NotFound);
        let wrapped = anyhow::Error::from(io).context("open foo");
        assert!(is_not_found(&wrapped));
        let other = anyhow::anyhow!("unrelated");
        assert!(!is_not_found(&other));
    }

    #[test]
    fn finds_dupes() {
        let dir = tempfile::tempdir().unwrap();
        let blk = vec![9u8; 4096];
        let mut a = blk.clone();
        a.extend_from_slice(&[1u8; 4096]);
        let mut b = blk.clone();
        b.extend_from_slice(&[2u8; 4096]);
        let files = [
            fixture(dir.path(), "a", &a, 4096),
            fixture(dir.path(), "b", &b, 4096),
        ];
        let stats = dedup(&files, DRY);
        assert_eq!(stats.candidates, 1);
        assert_eq!(stats.verified, 1);
        assert_eq!(stats.cloned, 0);
        assert_eq!(stats.mismatches, 0);
    }

    #[test]
    fn no_dupes() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            fixture(dir.path(), "a", &[1u8; 4096], 4096),
            fixture(dir.path(), "b", &[2u8; 4096], 4096),
        ];
        assert_eq!(dedup(&files, DRY).candidates, 0);
    }

    #[test]
    fn ignores_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let z = [0u8; 8192];
        let files = [
            fixture(dir.path(), "a", &z, 4096),
            fixture(dir.path(), "b", &z, 4096),
        ];
        assert_eq!(dedup(&files, DRY).candidates, 0);
    }

    #[test]
    fn readonly_source() {
        // Source is never written to; mode 0400 must not fail the open.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = [9u8; 4096];
        let files = [
            fixture(dir.path(), "ro", &data, 4096),
            fixture(dir.path(), "rw", &data, 4096),
        ];
        std::fs::set_permissions(&files[0].0, std::fs::Permissions::from_mode(0o400)).unwrap();
        let opts = Opts {
            dry_run: false,
            fideduperange: false,
        };
        let w = Worker::new(&files, opts);
        w.open(0, false).expect("read-only source must open");
        w.open(1, true).expect("writable dest must open");
    }

    #[test]
    fn stale_index_past_eof() {
        // File grew between stat() and the hash read: index has chunk
        // offsets past stat.size. Must skip them, not underflow.
        let dir = tempfile::tempdir().unwrap();
        let data = [9u8; 8192];
        let mut files = [
            fixture(dir.path(), "a", &data, 4096),
            fixture(dir.path(), "b", &data, 4096),
        ];
        for (_, h) in &mut files {
            h.stat.size = 0;
        }
        assert_eq!(dedup(&files, DRY).verified, 0);
    }

    // Two distinct chunk hashes that share the upper 8 bytes collide on
    // dup_key but must not become a dedup candidate.
    #[test]
    fn dup_key_collision() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = [
            fixture(dir.path(), "a", &[1u8; 4096], 4096),
            fixture(dir.path(), "b", &[2u8; 4096], 4096),
        ];
        files[0].1.hashes[0] = [0xaa; 16];
        files[1].1.hashes[0] = [0xbb; 16];
        files[1].1.hashes[0][8..].copy_from_slice(&[0xaa; 8]);
        assert_eq!(
            dup_key(4096, &files[0].1.hashes[0]),
            dup_key(4096, &files[1].1.hashes[0]),
        );
        assert_eq!(dedup(&files, DRY).candidates, 0);
    }

    #[test]
    fn cross_blksz() {
        let dir = tempfile::tempdir().unwrap();
        let data = [9u8; 4096];
        let files = [
            fixture(dir.path(), "a", &data, 4096),
            fixture(dir.path(), "b", &data, 8192),
        ];
        // Same bytes, same hash, different blksz: must not be a candidate.
        assert_eq!(files[0].1.hashes[0], hash_chunk(&data));
        assert_eq!(dedup(&files, DRY).candidates, 0);
    }
}
