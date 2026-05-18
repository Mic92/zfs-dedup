use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use zfs_dedup::{bloom, cache::Cache, clone, dedup, hasher, remount, walk};

// Returns freed pages to the OS (MADV_FREE); glibc's per-thread arenas
// retain freed memory and inflate RSS for the rest of the run.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Mix (fsid, ino) into one 64-bit key for the prune Bloom filter.
// SplitMix64-style finalizer so closely spaced inodes scatter.
fn prune_key(fsid: u64, ino: u64) -> u64 {
    let mut k = fsid.rotate_left(32) ^ ino;
    k = (k ^ (k >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    k = (k ^ (k >> 27)).wrapping_mul(0x94d049bb133111eb);
    k ^ (k >> 31)
}

const USAGE: &str = "\
usage: zfs-dedup [-n] [-c CACHE] [-j N] [DIR...]
  DIR...             directories to scan (default: all mounted ZFS datasets)
  -c, --cache PATH   hash cache (default: $XDG_CACHE_HOME/zfs-dedup/cache.redb)
  -n, --dry-run      don't modify anything
  -j, --jobs N       hashing threads (default: all cores)
  -f, --force        dedup even without FIDEDUPERANGE (racy verify+clone)
  -V, --version      print version
";

fn default_cache() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/var/cache"));
    base.join("zfs-dedup").join("cache.redb")
}

struct Args {
    cache: PathBuf,
    dry_run: bool,
    jobs: Option<usize>,
    force: bool,
    dirs: Vec<PathBuf>,
}

fn parse_args() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;
    let mut args = Args {
        cache: default_cache(),
        dry_run: false,
        jobs: None,
        force: false,
        dirs: vec![],
    };
    let mut p = lexopt::Parser::from_env();
    while let Some(arg) = p.next()? {
        match arg {
            Short('c') | Long("cache") => args.cache = p.value()?.into(),
            Short('n') | Long("dry-run") => args.dry_run = true,
            Short('j') | Long("jobs") => args.jobs = Some(p.value()?.parse()?),
            Short('f') | Long("force") => args.force = true,
            Short('h') | Long("help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Short('V') | Long("version") => {
                println!("zfs-dedup {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Value(v) => args.dirs.push(v.into()),
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprint!("zfs-dedup: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // Before rayon spawns workers so they inherit the namespace. Done in
    // dry-run too so the scan scope matches a real run; the namespace
    // and any remounts are invisible to the host. If it fails (no
    // CAP_SYS_ADMIN) we must not remount, or we'd alter the host.
    let private_ns = remount::enter_private_mount_ns()
        .inspect_err(|e| eprintln!("zfs-dedup: private mount namespace unavailable: {e:#}"))
        .is_ok();
    if let Some(n) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .ok();
    }
    match run(&args, private_ns) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE, // ran, but some files couldn't be processed
        Err(e) => {
            eprintln!("zfs-dedup: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

// The probe writes; try each dir until one accepts it so a read-only or
// full first dataset doesn't abort the run. The answer is per-kernel,
// not per-dataset, so the first definitive result wins.
fn probe_fideduperange(dirs: &BTreeSet<PathBuf>) -> bool {
    for d in dirs {
        match clone::probe_dedupe(d) {
            Ok(b) => return b,
            Err(e) => eprintln!("probe FIDEDUPERANGE in {}: {e}", d.display()),
        }
    }
    false
}

fn run(args: &Args, private_ns: bool) -> Result<bool> {
    if let Some(parent) = args.cache.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cache = Cache::open(&args.cache).with_context(|| {
        format!(
            "open cache {} (another zfs-dedup running, or stale lock?)",
            args.cache.display()
        )
    })?;

    // Every ZFS dataset is its own mount and the walk stops at mount
    // boundaries, so collect each dataset under the requested roots
    // separately or child datasets get silently skipped.
    let mounts = walk::zfs_mounts()?;
    let mut dirs = BTreeSet::new();
    if args.dirs.is_empty() {
        dirs.extend(mounts.iter().cloned());
    } else {
        for d in &args.dirs {
            let root =
                std::fs::canonicalize(d).with_context(|| format!("resolve {}", d.display()))?;
            dirs.extend(mounts.iter().filter(|m| m.starts_with(&root)).cloned());
            dirs.insert(root); // may be a subdir, not a mountpoint
        }
    }
    // Drop non-ZFS dirs before the FIDEDUPERANGE probe picks one.
    dirs.retain(|d| {
        let ok = walk::is_zfs(d);
        if !ok {
            eprintln!("skip {}: not a ZFS filesystem", d.display());
        }
        ok
    });
    // ro ZFS bind mounts (e.g., /nix/store on NixOS) sit over a writable
    // dataset; remount them rw in our private namespace so they can be
    // deduped. Truly read-only datasets stay ro and are skipped.
    if private_ns {
        remount::remount_rw_binds(&dirs)?;
    }
    dirs.retain(|d| {
        let ok = !remount::is_ro(d);
        if !ok {
            eprintln!("skip {}: read-only", d.display());
        }
        ok
    });
    if dirs.is_empty() {
        bail!(
            "nothing to scan: no mounted ZFS datasets on a writable, \
             block_cloning-capable pool (see skip messages above, if any)"
        );
    }
    eprintln!("scanning {} ZFS mountpoints", dirs.len());

    // FIDEDUPERANGE compares and clones under inode locks; without it
    // there's a window between our compare and the clone.
    let fideduperange = !args.dry_run && probe_fideduperange(&dirs);
    if !args.dry_run && !fideduperange && !args.force {
        bail!(
            "\
This ZFS doesn't support FIDEDUPERANGE, the in-kernel atomic
verify-and-clone (stock OpenZFS doesn't yet). The FICLONERANGE fallback
compares blocks in userspace before cloning; a write that lands between
the compare and the clone is silently lost.

This is safe when nothing is writing to the scanned files: idle
datasets, archives, read-only trees like /nix/store. Pass --force to
proceed."
        );
    }

    // Don't dedup our own cache file if it lives under a scanned dir; redb
    // preallocates zero pages that all hash equal and shift under us.
    let exclude: HashSet<_> = std::fs::metadata(&args.cache)
        .map(|m| (m.dev(), m.ino()))
        .into_iter()
        .collect();
    let opts = dedup::Opts {
        dry_run: args.dry_run,
        fideduperange,
    };

    // Cross-dataset cloning is impossible (FIDEDUPERANGE rejects with
    // EXDEV before ZFS sees the call), so the index never spans fsids.
    // Process one dataset at a time: peak RSS is bounded by the largest
    // dataset, not their sum.
    let mount_set: HashSet<&PathBuf> = mounts.iter().collect();
    let mut by_fsid: BTreeMap<u64, Vec<&PathBuf>> = BTreeMap::new();
    for d in &dirs {
        if let Ok(id) = walk::fsid(d) {
            by_fsid.entry(id).or_default().push(d);
        }
    }

    let mut stats = dedup::Stats::default();
    let mut total = 0u64;
    let mut hash_errors = 0usize;
    for (fsid, group) in by_fsid {
        let paths = walk::files(group.iter().copied(), &exclude);
        let n_files = paths.files.len();
        eprintln!("{}: found {n_files} files", group[0].display());

        let results = hasher::hash_files(&cache, paths)?;
        // Bloom is enough: prune may over-keep -- a false positive holds a
        // stale cache entry one run too long. ~1.2 B/file vs ~24 in a HashSet.
        let mut seen = bloom::Bloom::new(n_files as u64);
        let mut hits = 0usize;
        let hashed = results.filter_map(|p, r, arena| match r {
            Ok(h) => {
                seen.insert(prune_key(h.fsid, h.ino));
                if h.from_cache {
                    hits += 1;
                }
                Some(h)
            }
            // Files vanish mid-scan on a live system; not an error.
            Err(e) if dedup::is_not_found(&e) => None,
            Err(e) => {
                eprintln!("skip {}: {e:#}", p.to_path(arena).display());
                hash_errors += 1;
                None
            }
        });
        let group_total: u64 = hashed.files.iter().map(|(_, h)| h.size).sum();
        total += group_total;
        eprintln!(
            "  hashed {} files ({hits} from cache), {}",
            hashed.files.len(),
            human(group_total)
        );

        // Only prune datasets we scanned in full; a subdir scan covers part
        // of a dataset and must not evict siblings it never visited.
        if group.iter().all(|d| mount_set.contains(d)) {
            let pruned = cache.prune(|f, i| seen.contains(prune_key(f, i)), &[fsid].into())?;
            if pruned > 0 {
                eprintln!("  pruned {pruned} stale cache entries");
            }
        }
        drop(seen);

        stats += dedup::dedup(&hashed, &cache, opts)?;
    }

    if cache.compact_if_bloated()? {
        eprintln!("compacted cache");
    }

    let pct = if total > 0 {
        stats.bytes as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    let verb = if args.dry_run { "would save" } else { "saved" };
    eprintln!(
        "{verb} {} ({pct:.1}%) across {} blocks, {} mismatches, {} errors",
        human(stats.bytes),
        stats.verified,
        stats.mismatches,
        stats.errors,
    );
    Ok(hash_errors == 0 && stats.errors == 0)
}
