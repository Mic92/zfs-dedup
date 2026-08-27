# zfs-dedup

Reclaim space from duplicate files on ZFS without turning on
deduplication. It scans your datasets, finds blocks with identical
content, and reflinks them with block cloning.
zfs-dedup runs offline unlike zfs own deduplication,
it doesn't require additional memory, when it doesn't run.

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
- `FIDEDUPERANGE` for in-kernel verify+clone is in OpenZFS master
  (<https://github.com/openzfs/zfs/pull/18745>, not yet in a release).
  Older ZFS requires `--force` for the userspace verify path.
- Read-only ZFS bind mounts over a writable dataset (e.g.,
  `/nix/store` on NixOS) are remounted read-write in a private mount
  namespace. The host's mounts are not modified.

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
operation (`FIDEDUPERANGE`), otherwise something could write to one of
them in between. OpenZFS gained this ioctl in
<https://github.com/openzfs/zfs/pull/18745> (master, not yet released).
zfs-dedup uses it automatically when available.

On older ZFS, zfs-dedup falls back to a userspace compare followed by
`FICLONERANGE` and refuses unless you pass `--force` to accept the small
race window. Only use `--force` when you are sure that no process will
modify your data while `zfs-dedup` runs. The fallback also bumps the
mtime of deduped files, because ZFS treats a clone as a write.
`FIDEDUPERANGE` leaves timestamps untouched.

## Memory

zfs-dedup needs roughly 240 MiB per million files in the largest dataset
during the walk, dropping to about 65 MiB per million afterwards.
As datasets are scanned one at a time, only the biggest one matters.

| files in largest dataset | peak RSS | after the walk |
|---|---|---|
| 1 M | ~240 MiB | ~65 MiB |
| 10 M | ~2.4 GiB | ~650 MiB |
| 50 M | ~12 GiB | ~3.2 GiB |

## Limitations

- ZFS only. For btrfs or XFS use [bees](https://github.com/Zygo/bees)
  or [duperemove](https://github.com/markfasheh/duperemove).
- Files must be on the same pool and share a recordsize to be cloned
  into each other.
- Cloned blocks share storage but show up twice in `du`. Check
  `zpool get bcloneused,bclonesaved` for actual savings, the tool also will report savings

## License

MIT
