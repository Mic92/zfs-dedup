//! FICLONERANGE ioctl wrapper.
//!
//! ZFS does not implement FIDEDUPERANGE (returns EOPNOTSUPP), so offline
//! dedup must verify equality in userspace and then clone the range.

use std::os::fd::{AsFd, AsRawFd};

use rustix::io;
use rustix::ioctl::{Opcode, Setter, ioctl, opcode};

/// Mirrors `struct file_clone_range` from `<linux/fs.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FileCloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

/// `FICLONERANGE` = `_IOW(0x94, 13, struct file_clone_range)`.
const FICLONERANGE: Opcode = opcode::write::<FileCloneRange>(0x94, 13);

/// Clone `len` bytes from `src` at `src_off` into `dst` at `dst_off`.
///
/// On ZFS, offsets and `len` must be multiples of the source dataset's
/// `recordsize` (except for the final block at EOF), and both files must
/// have the same blocksize. Returns `EINVAL` otherwise, `EXDEV` for
/// cross-pool or cross-encryption-root attempts.
pub fn clone_range<S: AsFd, D: AsFd>(
    src: S,
    src_off: u64,
    dst: D,
    dst_off: u64,
    len: u64,
) -> io::Result<()> {
    let arg = FileCloneRange {
        src_fd: i64::from(src.as_fd().as_raw_fd()),
        src_offset: src_off,
        src_length: len,
        dest_offset: dst_off,
    };
    // SAFETY: FICLONERANGE is a pointer-setter opcode; FileCloneRange
    // matches the kernel ABI layout exactly.
    unsafe { ioctl(dst, Setter::<FICLONERANGE, FileCloneRange>::new(arg)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn clone_range_on_tmpfs_returns_enotsup_or_works() {
        // tmpfs has no reflink; expect EOPNOTSUPP/ENOTTY/EBADF/EINVAL —
        // the point is the ioctl is wired up, not that it succeeds here.
        let dir = std::env::temp_dir();
        let opts = || {
            let mut o = std::fs::OpenOptions::new();
            o.read(true).write(true).create(true).truncate(true);
            o
        };
        let mut a = opts().open(dir.join("zfs-dedup-test-a")).unwrap();
        let mut b = opts().open(dir.join("zfs-dedup-test-b")).unwrap();
        a.write_all(&[0u8; 4096]).unwrap();
        b.write_all(&[0u8; 4096]).unwrap();
        a.sync_all().unwrap();
        b.sync_all().unwrap();
        let r = clone_range(&a, 0, &b, 0, 4096);
        // Either succeeds (btrfs/xfs/zfs tmp) or fails with a known errno.
        if let Err(e) = r {
            assert!(
                matches!(
                    e,
                    io::Errno::OPNOTSUPP | io::Errno::NOTTY | io::Errno::INVAL | io::Errno::XDEV,
                ),
                "unexpected errno: {e}"
            );
        }
        let _ = std::fs::remove_file(dir.join("zfs-dedup-test-a"));
        let _ = std::fs::remove_file(dir.join("zfs-dedup-test-b"));
    }
}
