use std::io::Write;
use std::path::{Path, PathBuf};

use divan::{Bencher, black_box, counter::BytesCount};
use zfs_dedup::cache::Cache;
use zfs_dedup::dedup::{build_index, dedup};
use zfs_dedup::hasher::{Hashed, Stat, hash_chunk, hash_file, hash_files};

fn main() {
    divan::main();
}

#[divan::bench(args = [4096, 16384, 131072, 1048576])]
fn chunk(b: Bencher, size: usize) {
    let buf = vec![0xa5u8; size];
    b.counter(BytesCount::new(size))
        .bench(|| hash_chunk(black_box(&buf)));
}

#[divan::bench(args = [1, 16, 256])]
fn file_mb(b: Bencher, mb: u64) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f");
    let f = std::fs::File::create(&p).unwrap();
    f.set_len(mb * 1024 * 1024).unwrap(); // sparse, so this is best-case IO
    b.counter(BytesCount::new(mb * 1024 * 1024))
        .bench(|| hash_file(black_box(&p), 131072).unwrap());
}

// End-to-end: walk -> hash (cold cache) -> index -> verify (dry-run).
// Files share `dup_pct` of their 128K chunks. The cold-cache pass is the
// realistic worst case; warm-cache only stats.
#[divan::bench(args = [(8, 4, 0), (8, 4, 50), (8, 4, 100), (64, 1, 50)])]
fn pipeline(b: Bencher, (n_files, mb_each, dup_pct): (usize, u64, u8)) {
    const BLK: usize = 131072;
    let dir = tempfile::tempdir().unwrap();
    let chunks_per_file = (mb_each * 1024 * 1024) as usize / BLK;
    let n_dup = chunks_per_file * dup_pct as usize / 100;
    let shared: Vec<[u8; BLK]> = (0..n_dup).map(|i| pattern(i as u64)).collect();
    for f in 0..n_files {
        let mut w = std::fs::File::create(dir.path().join(format!("f{f}"))).unwrap();
        for s in &shared {
            w.write_all(s).unwrap();
        }
        for c in n_dup..chunks_per_file {
            w.write_all(&pattern((f * chunks_per_file + c) as u64 | 1 << 63))
                .unwrap();
        }
    }
    let cache_dir = tempfile::tempdir().unwrap();
    let total = n_files as u64 * mb_each * 1024 * 1024;
    b.counter(BytesCount::new(total)).bench(|| {
        let stats = run_pipeline(dir.path(), &cache_dir.path().join("c.redb"));
        std::fs::remove_file(cache_dir.path().join("c.redb")).unwrap();
        black_box(stats)
    });
}

fn run_pipeline(data_dir: &Path, cache_path: &Path) -> zfs_dedup::dedup::Stats {
    let cache = Cache::open(cache_path).unwrap();
    // Bench fixtures live in a tmpdir, not ZFS, so don't go through
    // walk::files() which would refuse them. The fixture dir is flat.
    let mut paths: Vec<_> = std::fs::read_dir(data_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| (p, 0u64))
        .collect();
    paths.sort();
    let hashed: Vec<_> = hash_files(&cache, paths)
        .unwrap()
        .into_iter()
        .map(|(p, r)| (p, r.unwrap()))
        .collect();
    dedup(
        &hashed,
        &cache,
        zfs_dedup::dedup::Opts {
            dry_run: true,
            fideduperange: false,
        },
    )
    .unwrap()
}

fn pattern(seed: u64) -> [u8; 131072] {
    let mut out = [0u8; 131072];
    let mut x = seed.wrapping_mul(0x9e3779b97f4a7c15) | 1;
    for c in out.chunks_exact_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.copy_from_slice(&x.to_le_bytes());
    }
    out
}

#[divan::bench(args = [1_000, 100_000])]
fn index(b: Bencher, n_chunks: u32) {
    // Synthetic file with all-unique chunks: worst case for the hashmap.
    let stat = Stat {
        fsid: 1,
        ino: 1,
        size: n_chunks as u64 * 131072,
        mtime_ns: 0,
        ctime_ns: 0,
        blksz: 131072,
    };
    // Realistic 128-bit keys: production hashes are uniform across all
    // bytes, and ChunkKeyHasher relies on that.
    let hashes: Vec<_> = (0..n_chunks)
        .map(|i| hash_chunk(&i.to_le_bytes()))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::open(&dir.path().join("c.redb")).unwrap();
    cache
        .put_many([(stat.fsid, stat.ino, stat.entry(&hashes))])
        .unwrap();
    let files = vec![(
        PathBuf::from("x"),
        Hashed {
            stat,
            from_cache: false,
        },
    )];
    b.bench(|| build_index(black_box(&files), &cache).unwrap());
}
