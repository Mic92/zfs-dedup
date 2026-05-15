use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use zfs_dedup::{cache::Cache, dedup, hasher, walk};

const USAGE: &str = "\
usage: zfs-dedup [-n] [-c CACHE] [-j N] DIR...
  -c, --cache PATH   hash cache (default: zfs-dedup.redb)
  -n, --dry-run      don't modify anything
  -j, --jobs N       hashing threads (default: all cores)
  -f, --force        scan non-ZFS filesystems too
";

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
        cache: "zfs-dedup.redb".into(),
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
            Value(v) => args.dirs.push(v.into()),
            _ => return Err(arg.unexpected()),
        }
    }
    if args.dirs.is_empty() {
        return Err("no directories given".into());
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
    if let Err(e) = run(&args) {
        eprintln!("zfs-dedup: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run(args: &Args) -> Result<()> {
    let cache = Cache::open(&args.cache)?;
    // Don't dedup our own cache file if it lives under a scanned dir; redb
    // preallocates zero pages that all hash equal and shift under us.
    let cache_abs = std::fs::canonicalize(&args.cache).ok();
    let mut paths = walk::files(&args.dirs, !args.force);
    paths.retain(|p| std::fs::canonicalize(p).ok() != cache_abs);
    eprintln!("found {} files", paths.len());

    let results = hasher::hash_files(&cache, &paths)?;
    let mut hashed = Vec::with_capacity(results.len());
    let mut seen = HashSet::new();
    let mut hits = 0usize;
    for (p, r) in results {
        match r {
            Ok(h) => {
                seen.insert((h.stat.dev, h.stat.ino));
                if h.from_cache {
                    hits += 1;
                }
                hashed.push((p, h));
            }
            Err(e) => eprintln!("skip {}: {e:#}", p.display()),
        }
    }
    eprintln!("hashed {} files ({hits} from cache)", hashed.len());

    let pruned = cache.prune(&seen)?;
    if pruned > 0 {
        eprintln!("pruned {pruned} stale cache entries");
    }

    let stats = dedup::dedup(&hashed, args.dry_run);
    let action = if args.dry_run {
        "would clone"
    } else {
        "cloned"
    };
    eprintln!(
        "{} candidates, {} verified, {action} {} ({} bytes), {} mismatches, {} errors",
        stats.candidates, stats.verified, stats.cloned, stats.bytes, stats.mismatches, stats.errors
    );
    Ok(())
}
