use std::path::PathBuf;

use divan::{Bencher, black_box, counter::BytesCount};
use zfs_dedup::dedup::build_index;
use zfs_dedup::hasher::{Hashed, Stat, hash_chunk, hash_file};

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

#[divan::bench(args = [1_000, 100_000])]
fn index(b: Bencher, n_chunks: u32) {
    // Synthetic file with all-unique chunks: worst case for the hashmap.
    let stat = Stat {
        dev: 1,
        ino: 1,
        size: n_chunks as u64 * 131072,
        mtime_ns: 0,
        ctime_ns: 0,
        blksz: 131072,
    };
    let hashes = (0..n_chunks).map(|i| (i as u128).to_le_bytes()).collect();
    let files = vec![(
        PathBuf::from("x"),
        Hashed {
            stat,
            hashes,
            from_cache: false,
        },
    )];
    b.bench(|| build_index(black_box(&files)));
}
