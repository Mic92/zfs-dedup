use std::os::fd::{AsFd, AsRawFd};

use rustix::io;
use rustix::ioctl::{Opcode, Setter, ioctl, opcode};

// struct file_clone_range from <linux/fs.h>
#[repr(C)]
struct FileCloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

// _IOW(0x94, 13, struct file_clone_range)
const FICLONERANGE: Opcode = opcode::write::<FileCloneRange>(0x94, 13);

// ZFS doesn't implement FIDEDUPERANGE, only FICLONERANGE. So we have to
// verify equality in userspace ourselves and then clone. Offsets and len
// must be recordsize-aligned (except the EOF tail) and both files must
// share blocksize, or we get EINVAL.
pub fn clone_range(
    src: impl AsFd,
    src_off: u64,
    dst: impl AsFd,
    dst_off: u64,
    len: u64,
) -> io::Result<()> {
    let arg = FileCloneRange {
        src_fd: src.as_fd().as_raw_fd().into(),
        src_offset: src_off,
        src_length: len,
        dest_offset: dst_off,
    };
    unsafe { ioctl(dst, Setter::<FICLONERANGE, _>::new(arg)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ioctl_is_wired_up() {
        // tmpfs has no reflink, so we just check it fails with a sane errno
        // rather than e.g. EBADF from a botched struct layout.
        let dir = tempfile::tempdir().unwrap();
        let open = |n: &str| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(dir.path().join(n))
                .unwrap()
        };
        let mut a = open("a");
        let mut b = open("b");
        a.write_all(&[0; 4096]).unwrap();
        b.write_all(&[0; 4096]).unwrap();
        a.sync_all().unwrap();
        b.sync_all().unwrap();
        if let Err(e) = clone_range(&a, 0, &b, 0, 4096) {
            assert!(
                matches!(
                    e,
                    io::Errno::OPNOTSUPP | io::Errno::NOTTY | io::Errno::INVAL | io::Errno::XDEV
                ),
                "unexpected errno: {e}"
            );
        }
    }
}
