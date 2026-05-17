use std::collections::{BTreeSet, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use zfs_dedup::{cache::Cache, clone, dedup, hasher, walk};

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
    if let Some(n) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .ok();
    }
    match run(&args) {
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

fn run(args: &Args) -> Result<bool> {
    if let Some(parent) = args.cache.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cache =
        Cache::open(&args.cache).with_context(|| format!("open cache {}", args.cache.display()))?;

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
    if dirs.is_empty() {
        bail!("no mounted ZFS datasets found");
    }
    eprintln!("scanning {} ZFS mountpoints", dirs.len());

    // FIDEDUPERANGE compares and clones under inode locks; without it
    // there's a window between our compare and the clone.
    let fideduperange =
        !args.dry_run && clone::probe_dedupe(dirs.first().expect("dirs non-empty"))?;
    if !args.dry_run && !fideduperange && !args.force {
        bail!(
            "kernel lacks FIDEDUPERANGE; falling back to FICLONERANGE is \
             racy against concurrent writers. Pass --force to do it anyway."
        );
    }

    // Don't dedup our own cache file if it lives under a scanned dir; redb
    // preallocates zero pages that all hash equal and shift under us.
    let exclude: HashSet<_> = std::fs::metadata(&args.cache)
        .map(|m| (m.dev(), m.ino()))
        .into_iter()
        .collect();
    let paths = walk::files(&dirs, &exclude);
    eprintln!("found {} files", paths.len());

    let results = hasher::hash_files(&cache, &paths)?;
    let mut hashed = Vec::with_capacity(results.len());
    let mut seen = HashSet::new();
    let mut hits = 0usize;
    let mut hash_errors = 0usize;
    for (p, r) in results {
        match r {
            Ok(h) => {
                seen.insert((h.stat.fsid, h.stat.ino));
                if h.from_cache {
                    hits += 1;
                }
                hashed.push((p, h));
            }
            // Files vanish mid-scan on a live system; not an error.
            Err(e) if dedup::is_not_found(&e) => {}
            Err(e) => {
                eprintln!("skip {}: {e:#}", p.display());
                hash_errors += 1;
            }
        }
    }
    let total: u64 = hashed.iter().map(|(_, h)| h.stat.size).sum();
    eprintln!(
        "hashed {} files ({hits} from cache), {} total",
        hashed.len(),
        human(total)
    );

    // Only prune datasets we scanned in full; a subdir scan covers part
    // of a dataset and must not evict siblings it never visited.
    let mount_set: HashSet<&PathBuf> = mounts.iter().collect();
    let scanned_fsids: HashSet<u64> = dirs
        .iter()
        .filter(|d| mount_set.contains(d))
        .filter_map(|d| walk::fsid(d).ok())
        .collect();
    let pruned = cache.prune(&seen, &scanned_fsids)?;
    if pruned > 0 {
        eprintln!("pruned {pruned} stale cache entries");
    }

    let stats = dedup::dedup(
        &hashed,
        dedup::Opts {
            dry_run: args.dry_run,
            fideduperange,
        },
    );
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
