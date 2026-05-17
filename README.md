# zfs-dedup

Reclaim space from duplicate files on ZFS without turning on
deduplication. Scans your datasets, finds blocks with identical
content, and reflinks them with block cloning. Runs offline, no DDT,
no extra RAM.

## Install

```
nix run github:Mic92/zfs-dedup
```

or `cargo build --release`. Needs a ZFS pool with the `block_cloning`
feature enabled and Linux.

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
