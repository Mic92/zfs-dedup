use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use rayon::prelude::*;

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

// Index entries dominate working set on big trees; keep this small.
// u32 chunk caps a single file at 4G chunks (16 TiB at 4 KiB recordsize,
// 512 TiB at default 128 KiB); beyond that build_index asserts.
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
    fn write_u64(&mut self, n: u64) {
        self.0 ^= n.wrapping_mul(0x9e3779b97f4a7c15);
    }
}

// One candidate group per (blksz, fsid, hash). zfs_clone_range refuses
// cross-blocksize clones, and the VFS refuses cross-superblock (each
// dataset is one) FIDEDUPERANGE/FICLONERANGE with EXDEV, so neither
// pair can ever clone.
pub type Index = HashMap<(u32, u64, ChunkHash), Vec<Loc>, BuildHasherDefault<ChunkKeyHasher>>;

// Truncated key for the seen/dup pre-filter; halves its footprint.
// A collision only marks a singleton as a possible dup; pass 2 indexes
// by full key so the false group has one location and is a no-op.
fn dup_key(blksz: u32, fsid: u64, h: &ChunkHash) -> u64 {
    u64::from_le_bytes(h[8..16].try_into().expect("16-byte hash"))
        ^ u64::from(blksz).wrapping_mul(0x9e3779b97f4a7c15)
        ^ fsid.wrapping_mul(0xff51afd7ed558ccd)
}

// `seen` only needs to answer "have I seen this key before?" and may
// over-answer: a false positive marks a singleton as a possible dup,
// which becomes a one-element group that the trailing retain drops.
// A Bloom filter at 1% FP costs ~1.2 bytes/chunk vs ~16 for a HashSet.
struct Bloom {
    bits: Box<[u64]>,
    mask: u64,
}

impl Bloom {
    const K: u32 = 7; // ln(2) * bits/key for 1% FP

    // ~10 bits/key, rounded up to a power of two so position math is a
    // mask instead of a modulo.
    fn new(n_keys: u64) -> Self {
        let bits = (n_keys * 10).next_power_of_two().max(64);
        Self {
            bits: vec![0u64; (bits / 64) as usize].into_boxed_slice(),
            mask: bits - 1,
        }
    }

    // Returns whether `k` was already (probably) present, then marks it.
    // Kirsch-Mitzenmacher double hashing: position_i = h1 + i*h2. With
    // `mask` a power of two only the low bits matter, so h1 and h2 must
    // be independent there; split k's halves rather than mixing it,
    // which keeps the low bits correlated and quintuples the FP rate.
    fn check_insert(&mut self, k: u64) -> bool {
        let step = (k >> 32) | 1;
        // No early exit: every bit must be set even when `present` is
        // already false.
        let mut present = true;
        for i in 0..Self::K {
            let pos = k.wrapping_add(u64::from(i).wrapping_mul(step)) & self.mask;
            let word = &mut self.bits[pos as usize / 64];
            let bit = 1u64 << (pos % 64);
            present &= *word & bit != 0;
            *word |= bit;
        }
        present
    }
}

// Two passes: mark which keys repeat, then collect locations only for
// those. Building a Vec per chunk and discarding the singletons would
// transiently allocate several times the final index size on low-dup
// trees.
//
// Per-file hashes are streamed from the cache instead of held in RAM;
// they were just written there during hashing, so the reads are all
// page-cache hits. This bounds the working set by the index, not by
// total chunk count.
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
    let mut seen = Bloom::new(n_chunks);
    let mut dup = Pre::default();
    each_chunk(files, cache, |_, _, blksz, fsid, hash| {
        let k = dup_key(blksz, fsid, hash);
        if seen.check_insert(k) {
            dup.insert(k);
        }
    })?;
    drop(seen);

    let mut idx = Index::default();
    each_chunk(files, cache, |fi, ci, blksz, fsid, hash| {
        if !dup.contains(&dup_key(blksz, fsid, hash)) {
            return;
        }
        idx.entry((blksz, fsid, *hash)).or_default().push(Loc {
            file: fi as u32,
            chunk: u32::try_from(ci).expect("file too large for index"),
        });
    })?;
    // Truncated-key collisions in `dup` make singletons here; drop them
    // so group() doesn't open+read a source with nothing to clone to.
    idx.retain(|_, v| v.len() > 1);
    Ok(idx)
}

// Visit every non-zero chunk of every file, fetching hashes from cache.
fn each_chunk(
    files: &[(FilePath, Hashed)],
    cache: &Cache,
    mut f: impl FnMut(usize, usize, u32, u64, &ChunkHash),
) -> Result<()> {
    for (fi, (_, h)) in files.iter().enumerate() {
        let Some(entry) = cache.get(h.fsid, h.ino)? else {
            continue;
        };
        for (ci, hash) in entry.hashes.iter().enumerate() {
            if *hash != ZERO_HASH {
                f(fi, ci, h.blksz, h.fsid, hash);
            }
        }
    }
    Ok(())
}

// Per-rayon-task state. Read buffers reused across candidates; no fd
// cache because n_workers * n_files would blow ulimit -n on big trees,
// and open() is dwarfed by the verify reads anyway. Two tasks may open
// the same file (independent fds) and clone into different offsets
// concurrently, which ZFS handles fine. Groups never share a
// (file, chunk) since each chunk has exactly one hash.
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

pub fn dedup(files: &Paths<Hashed>, cache: &Cache, opts: Opts) -> Result<Stats> {
    let idx = build_index(&files.files, cache)?;
    let groups: Vec<_> = idx.into_iter().collect();
    Ok(groups
        .into_par_iter()
        .fold(
            || Worker::new(files, opts),
            |mut w, ((blksz, ..), locs)| {
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

// 0 if `chunk` is past stat.size: file grew between stat() and the
// hash read, leaving stale entries in the index. Callers skip those.
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
        fn file(&self, name: &str, data: &[u8], blksz: u32, fsid: u64) -> (PathBuf, Hashed) {
            let hashes = {
                let p = self.dir.path().join(name);
                std::fs::write(&p, data).unwrap();
                hash_file(&p, blksz).unwrap()
            };
            self.synth(name, data.len() as u64, blksz, fsid, &hashes)
        }

        // File with caller-supplied chunk hashes (collision tests etc.).
        fn synth(
            &self,
            name: &str,
            size: u64,
            blksz: u32,
            fsid: u64,
            hashes: &[ChunkHash],
        ) -> (PathBuf, Hashed) {
            let p = self.dir.path().join(name);
            if !p.exists() {
                std::fs::write(&p, vec![0u8; size as usize]).unwrap();
            }
            let m = std::fs::metadata(&p).unwrap();
            let mut stat = Stat::from_metadata(&m, fsid);
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
    fn bloom_properties() {
        // Hash sequential ints to scatter bits like real dup_keys.
        let key =
            |i: u64| u64::from_le_bytes(hash_chunk(&i.to_le_bytes())[..8].try_into().unwrap());
        let n = 10_000u64;
        let mut b = Bloom::new(n);
        for i in 0..n {
            b.check_insert(key(i));
        }
        // No false negatives.
        assert!((0..n).all(|i| b.check_insert(key(i))));
        // FP rate near design target (1%); generous margin for variance.
        let fp = (n..2 * n).filter(|&i| b.check_insert(key(i))).count();
        assert!(fp < n as usize / 20, "fp={fp} of {n}");
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
        let files = [tx.file("a", &a, 4096, 0), tx.file("b", &b, 4096, 0)];
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
            tx.file("a", &[1u8; 4096], 4096, 0),
            tx.file("b", &[2u8; 4096], 4096, 0),
        ];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }

    #[test]
    fn ignores_zeros() {
        let tx = Tx::new();
        let z = [0u8; 8192];
        let files = [tx.file("a", &z, 4096, 0), tx.file("b", &z, 4096, 0)];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }

    #[test]
    fn readonly_source() {
        // Source is never written to; mode 0400 must not fail the open.
        use std::os::unix::fs::PermissionsExt;
        let tx = Tx::new();
        let data = [9u8; 4096];
        let files = [tx.file("ro", &data, 4096, 0), tx.file("rw", &data, 4096, 0)];
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
        let mut files = [tx.file("a", &data, 4096, 0), tx.file("b", &data, 4096, 0)];
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
        assert_eq!(dup_key(4096, 0, &ha), dup_key(4096, 0, &hb));
        let files = [
            tx.synth("a", 4096, 4096, 0, &[ha]),
            tx.synth("b", 4096, 4096, 0, &[hb]),
        ];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }

    // Each ZFS dataset is its own superblock; the kernel rejects cross-
    // superblock FIDEDUPERANGE/FICLONERANGE with EXDEV before ZFS sees
    // it. Files on different datasets must never be candidates.
    #[test]
    fn cross_dataset() {
        let tx = Tx::new();
        let data = [9u8; 4096];
        let files = [tx.file("a", &data, 4096, 0), tx.file("b", &data, 4096, 1)];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }

    #[test]
    fn cross_blksz() {
        let tx = Tx::new();
        let data = [9u8; 4096];
        // Same bytes, same hash, different blksz: must not be a candidate.
        let files = [
            tx.synth("a", 4096, 4096, 0, &[hash_chunk(&data)]),
            tx.synth("b", 4096, 8192, 0, &[hash_chunk(&data)]),
        ];
        assert_eq!(tx.dedup(files, DRY).candidates, 0);
    }
}
