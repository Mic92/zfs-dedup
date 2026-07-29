// NixOS bind-mounts /nix/store read-only over a writable ZFS dataset.
// Other distros do similar things to immutable trees. Without a
// writable path zfs_clone_range can't dedup. Enter a private mount
// namespace and remount such ro binds rw; the change is invisible to
// the host and gone when the process exits.

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustix::mount::{MountFlags, MountPropagationFlags, mount_change, mount_remount};
use rustix::thread::{UnshareFlags, unshare_unsafe};

// Best-effort: needs CAP_SYS_ADMIN, which we have anyway for ZFS dedup.
// Must run before rayon spawns its pool so workers inherit the namespace.
pub fn enter_private_mount_ns() -> Result<()> {
    // Safety: single-threaded at this point; nothing else shares our fds
    // or memory via clone().
    unsafe { unshare_unsafe(UnshareFlags::NEWNS) }.context("unshare(CLONE_NEWNS)")?;
    // The new namespace inherits shared propagation from the host;
    // mark it slave so our remounts don't leak back out.
    mount_change(
        "/",
        MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC,
    )
    .context("make / mount-slave")?;
    Ok(())
}

pub(crate) struct Mount {
    pub(crate) point: PathBuf,
    // Path within the source filesystem; "/" for a dataset root,
    // something else for a bind mount of a subtree.
    pub(crate) root: PathBuf,
    pub(crate) source: String,
    flags: MountFlags,
    ro: bool,
    super_rw: bool,
    pub(crate) fstype: String,
}

// mountinfo escapes space/tab/newline/backslash as \040 etc.
fn unescape(s: &str) -> PathBuf {
    let mut out = Vec::with_capacity(s.len());
    let mut b = s.bytes();
    while let Some(c) = b.next() {
        if c == b'\\'
            && let (Some(a), Some(b1), Some(b2)) = (b.next(), b.next(), b.next())
        {
            out.push((a - b'0') * 64 + (b1 - b'0') * 8 + (b2 - b'0'));
        } else {
            out.push(c);
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

// /proc/self/mountinfo line:
//   id parent maj:min root mountpoint opts [optional...] - fstype src superopts
pub(crate) fn parse_mountinfo(line: &str) -> Option<Mount> {
    let (head, tail) = line.split_once(" - ")?;
    let mut h = head.split(' ');
    let root = unescape(h.nth(3)?);
    let point = unescape(h.next()?);
    let opts = h.next()?;
    let mut t = tail.split(' ');
    let fstype = t.next()?.to_owned();
    let source = t.next()?.to_owned();
    let super_opts = t.next()?;
    Some(Mount {
        point,
        root,
        source,
        flags: opts.split(',').filter_map(opt_flag).collect(),
        ro: opts.split(',').any(|o| o == "ro"),
        super_rw: super_opts.split(',').any(|o| o == "rw"),
        fstype,
    })
}

// Per-mount flags we must preserve on a bind remount; the kernel rejects
// dropping security flags. ro is the one we want to clear.
fn opt_flag(opt: &str) -> Option<MountFlags> {
    Some(match opt {
        "nosuid" => MountFlags::NOSUID,
        "nodev" => MountFlags::NODEV,
        "noexec" => MountFlags::NOEXEC,
        "noatime" => MountFlags::NOATIME,
        "nodiratime" => MountFlags::NODIRATIME,
        "relatime" => MountFlags::RELATIME,
        "strictatime" => MountFlags::STRICTATIME,
        "sync" => MountFlags::SYNCHRONOUS,
        "dirsync" => MountFlags::DIRSYNC,
        "lazytime" => MountFlags::LAZYTIME,
        _ => return None,
    })
}

// Remount ro ZFS bind mounts under `roots` rw. Only ro binds over rw
// datasets qualify; truly read-only datasets and pools are left alone.
pub fn remount_rw_binds(roots: &BTreeSet<PathBuf>) -> Result<()> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    for m in mountinfo.lines().filter_map(parse_mountinfo) {
        let in_scope = roots
            .iter()
            .any(|r| m.point.starts_with(r) || r.starts_with(&m.point));
        if m.fstype != "zfs" || !m.ro || !m.super_rw || !in_scope {
            continue;
        }
        match mount_remount(&m.point, m.flags | MountFlags::BIND, "") {
            Ok(()) => eprintln!("remounted {} rw (private namespace)", m.point.display()),
            Err(e) => eprintln!("remount {}: {e}", m.point.display()),
        }
    }
    Ok(())
}

pub fn is_ro(p: &Path) -> bool {
    rustix::fs::statvfs(p)
        .map(|s| s.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ro_bind() {
        let l = "54 39 0:33 /nix/store /nix/store ro,nosuid,nodev,relatime shared:17 - zfs zroot/root/nixos rw,xattr,posixacl,casesensitive";
        let m = parse_mountinfo(l).unwrap();
        assert_eq!(m.point, Path::new("/nix/store"));
        assert_eq!(m.root, Path::new("/nix/store"));
        assert_eq!(m.source, "zroot/root/nixos");
        assert!(m.ro);
        assert!(m.super_rw);
        assert_eq!(m.fstype, "zfs");
        assert!(
            m.flags
                .contains(MountFlags::NOSUID | MountFlags::NODEV | MountFlags::RELATIME)
        );
        assert!(!m.flags.contains(MountFlags::NOEXEC));
    }

    #[test]
    fn parse_rw() {
        let l = "39 1 0:33 / / rw,relatime shared:1 - zfs zroot/root/nixos rw,xattr,posixacl";
        let m = parse_mountinfo(l).unwrap();
        assert!(!m.ro);
        assert!(m.super_rw);
    }

    #[test]
    fn parse_truly_ro() {
        // ro pool: super is also ro, must not be remounted.
        let l = "60 1 0:40 / /backup ro,relatime - zfs backup/data ro,xattr";
        let m = parse_mountinfo(l).unwrap();
        assert!(m.ro);
        assert!(!m.super_rw);
    }

    #[test]
    fn parse_garbage() {
        assert!(parse_mountinfo("").is_none());
        assert!(parse_mountinfo("not a mountinfo line").is_none());
    }

    #[test]
    fn unescape_octal() {
        assert_eq!(unescape("/a\\040b"), Path::new("/a b"));
        assert_eq!(unescape("/x\\011y\\134z"), Path::new("/x\ty\\z"));
        assert_eq!(unescape("/plain"), Path::new("/plain"));
    }
}
