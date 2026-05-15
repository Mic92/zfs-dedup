use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

const USAGE: &str = "\
usage: zfs-dedup [-n] [-c CACHE] [-j N] DIR...
  -c, --cache PATH   hash cache (default: zfs-dedup.redb)
  -n, --dry-run      don't modify anything
  -j, --jobs N       hashing threads (default: all cores)
";

struct Args {
    cache: PathBuf,
    dry_run: bool,
    jobs: Option<usize>,
    dirs: Vec<PathBuf>,
}

fn parse_args() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;
    let mut args = Args {
        cache: "zfs-dedup.redb".into(),
        dry_run: false,
        jobs: None,
        dirs: vec![],
    };
    let mut p = lexopt::Parser::from_env();
    while let Some(arg) = p.next()? {
        match arg {
            Short('c') | Long("cache") => args.cache = p.value()?.into(),
            Short('n') | Long("dry-run") => args.dry_run = true,
            Short('j') | Long("jobs") => args.jobs = Some(p.value()?.parse()?),
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
    eprintln!(
        "cache={:?} dry_run={} dirs={:?}",
        args.cache, args.dry_run, args.dirs
    );
    // TODO: walk -> hash -> index -> dedup
    Ok(())
}
