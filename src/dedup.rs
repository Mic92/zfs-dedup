use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::bloom::Bloom;
use crate::cache::{Cache, ChunkHash};
use crate::clone::{Dedupe, clone_range, dedupe_range};
use crate::hasher::{Hashed, NOFOLLOW_NONBLOCK, ZERO_HASH};
use crate::walk::{FilePath, Paths};

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

// One chunk in one file, identified by index into the file list and
// chunk number. 8 bytes -- the index holds millions of these. The u32
// chunk number caps a file at 4G chunks (16 TiB at 4 KiB recordsize);
// build_index asserts on overflow.
#[derive(Clone, Copy)]
pub struct Loc {
    file: u32,
    chunk: u32,
}

impl Loc {
    fn off(self, blksz: u64) -> u64 {
        self.chunk as u64 * blksz
    }
}

// Index keys already contain a uniform XXH3-128 hash, so the HashMap
// doesn't need SipHash on top: take 8 bytes of the chunk hash and fold
// in blksz.
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
    fn write_u64(&mut self, n: u64) {
        self.0 ^= n.wrapping_mul(0x9e3779b97f4a7c15);
    }
}

// Maps (blksz, hash) to the chunks that share that hash. blksz is in
// the key because zfs_clone_range rejects cross-blocksize clones.
pub type Index = HashMap<(u32, ChunkHash), Vec<Loc>, BuildHasherDefault<ChunkKeyHasher>>;

// Truncated index key for the pre-filter set: 8 bytes vs 20. Collisions
// are safe -- pass 2 re-keys by the full hash, so a colliding singleton
// becomes a one-Loc group and the trailing retain drops it.
fn dup_key(blksz: u32, h: &ChunkHash) -> u64 {
    u64::from_le_bytes(h[8..16].try_into().expect("16-byte hash"))
        ^ u64::from(blksz).wrapping_mul(0x9e3779b97f4a7c15)
}

// Group every chunk whose hash repeats. Each group is a set of blocks
// that should be byte-identical; the dedup phase clones each into one.
//
// Two passes over the chunk stream:
//   1. Bloom pre-filter -> `dup`: hashes seen at least twice.
//   2. For chunks in `dup`, record their Loc in `idx`.
//
// One pass would record every chunk and drop the singletons at the
// end, allocating several times the final index size on a low-dup tree.
//
// Hashes stream from the redb cache, not RAM. hash_files just wrote
// them so the reads are page-cache warm. RSS is bounded by duplicate
// chunks, not total chunks.
pub fn build_index(files: &[(FilePath, Hashed)], cache: &Cache) -> Result<Index> {
    assert!(
        u32::try_from(files.len()).is_ok(),
        "too many files for index"
    );
    type Pre = HashSet<u64, BuildHasherDefault<ChunkKeyHasher>>;
    // Upper bound; over-sizing only lowers the FP rate.
    let n_chunks: u64 = files
        .iter()
        .map(|(_, h)| h.size / u64::from(h.blksz.max(1)) + 1)
        .sum();
    // `seen` may over-answer: a false positive marks a singleton as a
    // possible dup -- a one-element group the trailing retain drops.
    let mut seen = Bloom::new(n_chunks);
    let mut dup = Pre::default();
    each_chunk(files, cache, |_, _, h, hash| {
        let k = dup_key(h.blksz, hash);
        if seen.check_insert(k) {
            dup.insert(k);
        }
    })?;
    drop(seen);

    let mut idx = Index::default();
    each_chunk(files, cache, |fi, ci, h, hash| {
        if !dup.contains(&dup_key(h.blksz, hash)) {
            return;
        }
        idx.entry((h.blksz, *hash)).or_default().push(Loc {
            file: fi as u32,
            chunk: u32::try_from(ci).expect("file too large for index"),
        });
    })?;
    // Bloom false positives and dup_key truncation collisions land
    // here as one-element groups; drop them so the dedup phase doesn't
    // open and read a source it has nothing to clone into.
    idx.retain(|_, v| v.len() > 1);
    Ok(idx)
}

// Visit every non-zero chunk of every file, fetching hashes from cache.
// Calls `f(file_idx, chunk_idx, hashed, chunk_hash)`.
fn each_chunk(
    files: &[(FilePath, Hashed)],
    cache: &Cache,
    mut f: impl FnMut(usize, usize, &Hashed, &ChunkHash),
) -> Result<()> {
    for (fi, (_, h)) in files.iter().enumerate() {
        let Some(entry) = cache.get(h.fsid, h.ino)? else {
            continue;
        };
        for (ci, hash) in entry.hashes.iter().enumerate() {
            if *hash != ZERO_HASH {
                f(fi, ci, h, hash);
            }
        }
    }
    Ok(())
}

// Per-rayon-task state for the dedup phase: each task processes one
// index group at a time, opening files on demand and reusing the two
// read buffers across groups.
//
// No fd cache: n_workers * n_files would blow `ulimit -n`, and open()
// is dwarfed by the verify reads.
//
// Two tasks may open the same file concurrently (different groups, same
// file). They get independent fds and clone into different offsets;
// ZFS handles that. Two groups can never touch the same (file, chunk)
// because a chunk has exactly one hash and so belongs to one group.
struct Worker<'a> {
    files: &'a Paths<Hashed>,
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
    fn new(files: &'a Paths<Hashed>, opts: Opts) -> Self {
        Self {
            files,
            opts,
            buf_a: Vec::new(),
            buf_b: Vec::new(),
            stats: Stats::default(),
        }
    }

    fn path(&self, i: usize) -> PathBuf {
        self.files.path(&self.files.files[i].0)
    }

    fn hashed(&self, i: usize) -> &Hashed {
        &self.files.files[i].1
    }

    fn open(&self, i: usize, write: bool) -> Result<File> {
        let p = self.path(i);
        File::options()
            .read(true)
            .write(write && !self.opts.dry_run)
            .custom_flags(NOFOLLOW_NONBLOCK)
            .open(&p)
            .with_context(|| format!("open {p:?}"))
    }

    fn group(&mut self, blksz: u64, locs: &[Loc]) {
        // locs[0] is the canonical source: open and read it once,
        // clone the rest from it.
        let src = locs[0];
        let len = chunk_len(self.hashed(src.file as usize), src.chunk as u64, blksz);
        if len == 0 {
            return; // past stat.size: file grew after stat
        }
        let src_off = src.off(blksz);
        let compare = self.opts.dry_run || !self.opts.fideduperange;

        // Source is read-only: dedup must work on files we can't modify.
        let prep = |w: &mut Self| -> Result<File> {
            let sf = w.open(src.file as usize, false)?;
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
                eprintln!(
                    "skip group {:?}+{src_off}: {e:#}",
                    self.path(src.file as usize)
                );
                self.stats.errors += 1;
                return;
            }
        };

        for &dst in &locs[1..] {
            if len != chunk_len(self.hashed(dst.file as usize), dst.chunk as u64, blksz) {
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
                        self.path(dst.file as usize),
                        dst.off(blksz),
                        self.path(src.file as usize),
                    );
                    self.stats.errors += 1;
                }
            }
        }
    }

    // Returns Some(bytes deduped) if the source and destination matched
    // and were cloned, None on a hash collision (data differed).
    //
    // With `compare`: read both ranges, memcmp, FICLONERANGE on match.
    // Without: FIDEDUPERANGE does the compare-and-clone in one ioctl,
    // atomically under the inode locks.
    fn verify_and_clone(
        &mut self,
        sf: &File,
        src_off: u64,
        dst: Loc,
        blksz: u64,
        len: u64,
        compare: bool,
    ) -> Result<Option<u64>> {
        let dst_off = dst.off(blksz);
        let df = self.open(dst.file as usize, true)?;

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

// Dedup duplicate chunks within `files`. The caller must pass files
// from a single ZFS dataset: the VFS rejects cross-superblock
// FIDEDUPERANGE with EXDEV, and the index does not partition by fsid.
pub fn dedup(files: &Paths<Hashed>, cache: &Cache, opts: Opts) -> Result<Stats> {
    Ok(build_index(&files.files, cache)?
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
        }))
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

// Bytes in chunk `chunk` of a file of `h.size` bytes; the last chunk is
// short. Returns 0 if the chunk's offset is at or past EOF: defensive
// against a stale index entry, which callers skip.
fn chunk_len(h: &Hashed, chunk: u64, blksz: u64) -> u64 {
    let off = chunk * blksz;
    h.size.saturating_sub(off).min(blksz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::{Stat, hash_chunk, hash_file};

    const DRY: Opts = Opts {
        dry_run: true,
        fideduperange: false,
    };

    // Hashes live in the cache; tests need one alongside the files.
    struct Tx {
        dir: tempfile::TempDir,
        cache: Cache,
    }

    impl Tx {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cache = Cache::open(&dir.path().join("cache.redb")).unwrap();
            Self { dir, cache }
        }

        // Real file with a fixed blksz, ignoring whatever st_blksize
        // the test filesystem happens to report.
        fn file(&self, name: &str, data: &[u8], blksz: u32) -> (PathBuf, Hashed) {
            let hashes = {
                let p = self.dir.path().join(name);
                std::fs::write(&p, data).unwrap();
                hash_file(&p, blksz).unwrap()
            };
            self.synth(name, data.len() as u64, blksz, &hashes)
        }

        // File with caller-supplied chunk hashes (collision tests etc.).
        fn synth(
            &self,
            name: &str,
            size: u64,
            blksz: u32,
            hashes: &[ChunkHash],
        ) -> (PathBuf, Hashed) {
            let p = self.dir.path().join(name);
            if !p.exists() {
                std::fs::write(&p, vec![0u8; size as usize]).unwrap();
            }
            let m = std::fs::metadata(&p).unwrap();
            let mut stat = Stat::from_metadata(&m, 0);
            stat.blksz = blksz;
            stat.size = size;
            self.cache
                .put_many([(stat.fsid, stat.ino, stat.entry(hashes))])
                .unwrap();
            (p, Hashed::new(&stat, false))
        }

        fn dedup(&self, files: impl IntoIterator<Item = (PathBuf, Hashed)>, opts: Opts) -> Stats {
            let paths: Paths<Hashed> = files.into_iter().collect();
            dedup(&paths, &self.cache, opts).unwrap()
        }
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
        let tx = Tx::new();
        let blk = vec![9u8; 4096];
        let mut a = blk.clone();
        a.extend_from_slice(&[1u8; 4096]);
        let mut b = blk.clone();
        b.extend_from_slice(&[2u8; 4096]);
        let files = [tx.file("a", &a, 4096), tx.file("b", &b, 4096)];
        let stats = tx.dedup(files, DRY);
        assert_eq!(stats.candidates, 1);
        assert_eq!(stats.verified, 1);
        assert_eq!(stats.cloned, 0);
        assert_eq!(stats.mismatches, 0);
    }

    #[test]
    fn no_dupes() {
        let tx = Tx::new();
        let files = [
            tx.file("a", &[1u8; 4096], 4096),
            tx.file("b", &[2u8; 4096], 4096),
        ];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }

    #[test]
    fn ignores_zeros() {
        let tx = Tx::new();
        let z = [0u8; 8192];
        let files = [tx.file("a", &z, 4096), tx.file("b", &z, 4096)];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }

    #[test]
    fn readonly_source() {
        // Source is never written to; mode 0400 must not fail the open.
        use std::os::unix::fs::PermissionsExt;
        let tx = Tx::new();
        let data = [9u8; 4096];
        let files = [tx.file("ro", &data, 4096), tx.file("rw", &data, 4096)];
        std::fs::set_permissions(&files[0].0, std::fs::Permissions::from_mode(0o400)).unwrap();
        let opts = Opts {
            dry_run: false,
            fideduperange: false,
        };
        let paths: Paths<Hashed> = files.into_iter().collect();
        let w = Worker::new(&paths, opts);
        w.open(0, false).expect("read-only source must open");
        w.open(1, true).expect("writable dest must open");
    }

    #[test]
    fn stale_index_past_eof() {
        // File shrank between stat() and the hash read: index has chunk
        // offsets past stat.size. Must skip them, not underflow.
        let tx = Tx::new();
        let data = [9u8; 8192];
        let mut files = [tx.file("a", &data, 4096), tx.file("b", &data, 4096)];
        for (_, h) in &mut files {
            h.size = 0;
        }
        assert_eq!(tx.dedup(files, DRY).verified, 0);
    }

    // Two distinct chunk hashes that share the upper 8 bytes collide on
    // dup_key but must not become a dedup candidate.
    #[test]
    fn dup_key_collision() {
        let tx = Tx::new();
        let ha = [0xaa; 16];
        let mut hb = [0xbb; 16];
        hb[8..].copy_from_slice(&[0xaa; 8]);
        assert_eq!(dup_key(4096, &ha), dup_key(4096, &hb));
        let files = [
            tx.synth("a", 4096, 4096, &[ha]),
            tx.synth("b", 4096, 4096, &[hb]),
        ];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }

    #[test]
    fn cross_blksz() {
        let tx = Tx::new();
        let data = [9u8; 4096];
        // Same bytes, same hash, different blksz: must not be a candidate.
        let files = [
            tx.synth("a", 4096, 4096, &[hash_chunk(&data)]),
            tx.synth("b", 4096, 8192, &[hash_chunk(&data)]),
        ];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }
}
