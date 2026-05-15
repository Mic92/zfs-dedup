use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cache::ChunkHash;
use crate::clone::clone_range;
use crate::hasher::Hashed;

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub candidates: usize,
    pub verified: usize,
    pub cloned: usize,
    pub bytes: u64,
    pub mismatches: usize,
    pub errors: usize,
}

#[derive(Clone, Copy)]
pub struct Loc {
    file: usize,
    chunk: u32,
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

// One candidate group per (blksz, hash). zfs_clone_range refuses
// cross-blocksize clones, so files with different blksz never share.
pub fn build_index(files: &[(PathBuf, Hashed)]) -> Index {
    let mut idx = Index::default();
    for (fi, (_, h)) in files.iter().enumerate() {
        for (ci, hash) in h.hashes.iter().enumerate() {
            idx.entry((h.stat.blksz, *hash)).or_default().push(Loc {
                file: fi,
                chunk: ci as u32,
            });
        }
    }
    idx.retain(|_, v| v.len() > 1);
    idx
}

struct Handles<'a> {
    files: &'a [(PathBuf, Hashed)],
    fds: Vec<Option<File>>,
    write: bool,
}

impl<'a> Handles<'a> {
    fn new(files: &'a [(PathBuf, Hashed)], write: bool) -> Self {
        Self {
            files,
            fds: (0..files.len()).map(|_| None).collect(),
            write,
        }
    }

    fn get(&mut self, i: usize) -> Result<&File> {
        if self.fds[i].is_none() {
            self.fds[i] = Some(
                File::options()
                    .read(true)
                    .write(self.write)
                    .open(&self.files[i].0)
                    .with_context(|| format!("open {:?}", self.files[i].0))?,
            );
        }
        Ok(self.fds[i].as_ref().expect("just set"))
    }
}

pub fn dedup(files: &[(PathBuf, Hashed)], dry_run: bool) -> Stats {
    let idx = build_index(files);
    let mut stats = Stats::default();
    let mut handles = Handles::new(files, !dry_run);

    for ((blksz, _), locs) in &idx {
        let blksz = *blksz as u64;
        // First location in the group is the canonical source; everything
        // else gets cloned from it.
        let src = locs[0];
        for &dst in &locs[1..] {
            // Same file, same offset: already shared.
            if src.file == dst.file && src.chunk == dst.chunk {
                continue;
            }
            stats.candidates += 1;
            let len = chunk_len(&files[src.file].1, src.chunk, blksz);
            if len != chunk_len(&files[dst.file].1, dst.chunk, blksz) {
                continue; // tail-vs-full mismatch
            }
            match verify_and_clone(&mut handles, src, dst, blksz, len, dry_run) {
                Ok(true) => {
                    stats.verified += 1;
                    if !dry_run {
                        stats.cloned += 1;
                    }
                    stats.bytes += len;
                }
                Ok(false) => stats.mismatches += 1,
                Err(e) => {
                    eprintln!(
                        "skip {:?}+{} <- {:?}+{}: {e:#}",
                        files[dst.file].0,
                        dst.chunk as u64 * blksz,
                        files[src.file].0,
                        src.chunk as u64 * blksz,
                    );
                    stats.errors += 1;
                }
            }
        }
    }
    stats
}

fn chunk_len(h: &Hashed, chunk: u32, blksz: u64) -> u64 {
    let off = chunk as u64 * blksz;
    (h.stat.size - off).min(blksz)
}

fn verify_and_clone(
    handles: &mut Handles,
    src: Loc,
    dst: Loc,
    blksz: u64,
    len: u64,
    dry_run: bool,
) -> Result<bool> {
    let src_off = src.chunk as u64 * blksz;
    let dst_off = dst.chunk as u64 * blksz;

    // No FIDEDUPERANGE on ZFS, so the compare/clone window is racy. We
    // re-read here rather than trusting the (possibly stale) cache hash.
    let mut a = vec![0u8; len as usize];
    let mut b = vec![0u8; len as usize];
    handles.get(src.file)?.read_exact_at(&mut a, src_off)?;
    handles.get(dst.file)?.read_exact_at(&mut b, dst_off)?;
    if a != b {
        return Ok(false);
    }
    if !dry_run {
        let sf = handles.get(src.file)?.try_clone()?;
        let df = handles.get(dst.file)?;
        clone_range(&sf, src_off, df, dst_off, len).context("FICLONERANGE")?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::{Stat, hash_chunk, hash_file};

    // Build a (path, Hashed) pair with a fixed blksz, ignoring whatever
    // st_blksize the test filesystem happens to report.
    fn fixture(dir: &std::path::Path, name: &str, data: &[u8], blksz: u32) -> (PathBuf, Hashed) {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        let m = std::fs::metadata(&p).unwrap();
        let mut stat = Stat::from_metadata(&m);
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
    fn dry_run_finds_duplicates() {
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
        let stats = dedup(&files, true);
        assert_eq!(stats.candidates, 1);
        assert_eq!(stats.verified, 1);
        assert_eq!(stats.cloned, 0);
        assert_eq!(stats.mismatches, 0);
    }

    #[test]
    fn nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            fixture(dir.path(), "a", &[1u8; 4096], 4096),
            fixture(dir.path(), "b", &[2u8; 4096], 4096),
        ];
        assert_eq!(dedup(&files, true).candidates, 0);
    }

    #[test]
    fn cross_blksz_never_groups() {
        let dir = tempfile::tempdir().unwrap();
        let data = [9u8; 4096];
        let files = [
            fixture(dir.path(), "a", &data, 4096),
            fixture(dir.path(), "b", &data, 8192),
        ];
        // Same bytes, same hash, different blksz: must not be a candidate.
        assert_eq!(files[0].1.hashes[0], hash_chunk(&data));
        assert_eq!(dedup(&files, true).candidates, 0);
    }
}
