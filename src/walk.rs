use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

// Collect regular files. Hardlinked sets are collapsed to one path: same
// inode means already-shared storage, and zfs_clone_range would just hit
// the same dnode. Symlinks not followed.
pub fn files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        walk_root(root, &mut seen, &mut out);
    }
    out
}

fn walk_root(root: &Path, seen: &mut HashSet<(u64, u64)>, out: &mut Vec<PathBuf>) {
    for entry in jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("walk: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("walk: {}: {e}", entry.path().display());
                continue;
            }
        };
        if seen.insert((meta.dev(), meta.ino())) {
            out.push(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("a"), b"x").unwrap();
        std::fs::write(p.join("b"), b"y").unwrap();
        std::fs::hard_link(p.join("a"), p.join("a2")).unwrap();
        std::fs::create_dir(p.join("sub")).unwrap();
        std::fs::write(p.join("sub/c"), b"z").unwrap();
        std::os::unix::fs::symlink(p.join("a"), p.join("alink")).unwrap();

        let mut got: Vec<_> = files(std::slice::from_ref(&p.to_path_buf()))
            .into_iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        got.sort();
        // a/a2 collapse to one (whichever jwalk hits first), plus b and sub/c.
        assert_eq!(got.len(), 3);
        assert!(got.contains(&"b".into()));
        assert!(got.contains(&"c".into()));
        assert!(got.contains(&"a".into()) || got.contains(&"a2".into()));
    }
}
