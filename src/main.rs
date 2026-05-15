use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

const USAGE: &str = "\
Usage: zfs-dedup [OPTIONS] <DIR>...

Offline block-level deduplication for ZFS via FICLONERANGE.

Options:
  -c, --cache <PATH>   Hash cache file (default: ./zfs-dedup.redb)
  -n, --dry-run        Report what would be deduped, do not modify files
  -j, --jobs <N>       Parallel hashing threads (default: all cores)
  -h, --help           Show this help
";

#[derive(Debug)]
struct Args {
    cache: PathBuf,
    dry_run: bool,
    jobs: Option<usize>,
    dirs: Vec<PathBuf>,
}

fn parse_args() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;
    let mut cache = PathBuf::from("zfs-dedup.redb");
    let mut dry_run = false;
    let mut jobs = None;
    let mut dirs = Vec::new();

    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Short('c') | Long("cache") => cache = PathBuf::from(parser.value()?),
            Short('n') | Long("dry-run") => dry_run = true,
            Short('j') | Long("jobs") => jobs = Some(parser.value()?.parse()?),
            Short('h') | Long("help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Value(v) => dirs.push(PathBuf::from(v)),
            _ => return Err(arg.unexpected()),
        }
    }
    if dirs.is_empty() {
        return Err("missing required argument: <DIR>".into());
    }
    Ok(Args {
        cache,
        dry_run,
        jobs,
        dirs,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
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
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run(args: &Args) -> Result<()> {
    eprintln!(
        "zfs-dedup: cache={:?} dry_run={} dirs={:?}",
        args.cache, args.dry_run, args.dirs
    );
    // TODO: walk -> hash -> index -> dedup
    Ok(())
}
