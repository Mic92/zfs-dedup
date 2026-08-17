use std::collections::HashSet;
use std::path::PathBuf;

fn main() {
    let root: PathBuf = std::env::args().nth(1).expect("usage: walkmem DIR").into();
    let paths = zfs_dedup::walk::files(&[root], &HashSet::new());
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    unsafe { libc::malloc_trim(0) };
    let vmrss = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmRSS"))
        .and_then(|l| l.split_whitespace().nth(1).map(String::from))
        .unwrap();
    println!(
        "files: {}, peak RSS: {} MiB, retained RSS: {} MiB",
        paths.files.len(),
        ru.ru_maxrss / 1024,
        vmrss.parse::<u64>().unwrap() / 1024
    );
}
