# zfs-dedup

Reclaim space from duplicate files on ZFS without turning on
deduplication. Scans your datasets, finds blocks with identical
content, and reflinks them with block cloning. Runs offline, no DDT,
no extra RAM.

```
% sudo zfs-dedup ~/
scanning 1 ZFS mountpoints
found 8329094 files
hashed 8329094 files (4977666 from cache), 882.3 GiB total
pruned 97 stale cache entries
saved 79.0 GiB (9.0%) across 3860361 blocks, 0 mismatches, 0 errors
```

## Install

```
nix run github:Mic92/zfs-dedup
```

or `cargo build --release`.

## Requirements

- Linux
- OpenZFS **2.2.0+** with the `block_cloning` pool feature enabled
  (`zpool upgrade` or `zpool set feature@block_cloning=enabled`)
- OpenZFS **2.3.0+** recommended: hash reads use `O_DIRECT` to avoid
  evicting your hot ARC during a cold scan. Older ZFS silently falls
  back to buffered reads.
- `FIDEDUPERANGE` for in-kernel verify+clone needs a patched ZFS;
  stock ZFS requires `--force` for the userspace verify path.

## Usage

```
# Dry run, all mounted ZFS datasets
zfs-dedup -n

# Dedup specific paths
zfs-dedup ~/photos ~/backup

# Limit hashing threads
zfs-dedup -j 4 /tank
```

```
usage: zfs-dedup [-n] [-c CACHE] [-j N] [DIR...]
  DIR...             directories to scan (default: all mounted ZFS datasets)
  -c, --cache PATH   hash cache (default: $XDG_CACHE_HOME/zfs-dedup/cache.redb)
  -n, --dry-run      don't modify anything
  -j, --jobs N       hashing threads (default: all cores)
  -f, --force        dedup even without FIDEDUPERANGE (racy verify+clone)
  -V, --version      print version
```

Re-runs are cheap: file hashes are cached and only files that changed
get rehashed.

## Why it asks for --force

Deduping safely needs to compare two ranges and clone them in one
operation, otherwise something could write to one of them in between.
Stock OpenZFS doesn't have an ioctl that does both, so by default
zfs-dedup refuses unless you pass `--force` to accept the small race
window.

For the safe path, run a kernel module built from
<https://github.com/Mic92/zfs/tree/fideduperange>. It adds
FIDEDUPERANGE support, which zfs-dedup uses automatically when
available.

## Limitations

- ZFS only. For btrfs or XFS use [bees](https://github.com/Zygo/bees)
  or [duperemove](https://github.com/markfasheh/duperemove).
- Files must be on the same pool and share a recordsize to be cloned
  into each other.
- Cloned blocks share storage but show up twice in `du`; check
  `zpool get bcloneused,bclonesaved` for actual savings.
