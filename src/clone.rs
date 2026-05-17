use std::ffi::c_void;
use std::os::fd::{AsFd, AsRawFd};

use rustix::io;
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, Setter, ioctl, opcode};

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

// struct file_dedupe_range from <linux/fs.h>. `info` is a flexible array
// member, so the opcode is sized on the header alone but we pass a bigger
// buffer with one info entry appended.
#[repr(C)]
struct FileDedupeRange {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
}

#[repr(C)]
struct FileDedupeRangeInfo {
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved: u32,
}

#[repr(C)]
struct DedupeOne {
    range: FileDedupeRange,
    info: FileDedupeRangeInfo,
}

// _IOWR(0x94, 54, struct file_dedupe_range)
const FIDEDUPERANGE: Opcode = opcode::read_write::<FileDedupeRange>(0x94, 54);

const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;

struct Fideduperange(DedupeOne);

unsafe impl Ioctl for Fideduperange {
    type Output = i32;
    const IS_MUTATING: bool = true;

    fn opcode(&self) -> Opcode {
        FIDEDUPERANGE
    }

    fn as_ptr(&mut self) -> *mut c_void {
        (&raw mut self.0).cast()
    }

    unsafe fn output_from_ptr(_out: IoctlOutput, ptr: *mut c_void) -> io::Result<Self::Output> {
        Ok(unsafe { (*ptr.cast::<DedupeOne>()).info.status })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Dedupe {
    Same,
    Differs,
    Unsupported,
}

// In-kernel compare-and-clone. Stock ZFS returns EOPNOTSUPP.
pub fn dedupe_range(
    src: impl AsFd,
    src_off: u64,
    dst: impl AsFd,
    dst_off: u64,
    len: u64,
) -> io::Result<Dedupe> {
    let arg = DedupeOne {
        range: FileDedupeRange {
            src_offset: src_off,
            src_length: len,
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
        },
        info: FileDedupeRangeInfo {
            dest_fd: dst.as_fd().as_raw_fd().into(),
            dest_offset: dst_off,
            bytes_deduped: 0,
            status: 0,
            reserved: 0,
        },
    };
    // The kernel reports per-target errors via info.status, not the
    // ioctl return.
    let status = match unsafe { ioctl(&src, Fideduperange(arg)) } {
        Ok(s) => s,
        Err(e) => -e.raw_os_error(),
    };
    match status {
        0 => Ok(Dedupe::Same),
        FILE_DEDUPE_RANGE_DIFFERS => Ok(Dedupe::Differs),
        _ => match io::Errno::from_raw_os_error(-status) {
            io::Errno::NOTTY | io::Errno::OPNOTSUPP | io::Errno::NOSYS => Ok(Dedupe::Unsupported),
            e => Err(e),
        },
    }
}

// The kernel short-circuits len=0 dedups before reaching the fs callback,
// so we have to do a real one to find out if FIDEDUPERANGE works. Two
// 128 KiB temp files get the same blksz for any recordsize, so the
// whole-file dedup is always aligned. Incompressible data so the blocks
// land on disk instead of as embedded BPs (which can't be shared).
pub fn probe_dedupe(dir: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Write;

    const LEN: usize = 128 * 1024;
    let mut data = vec![0u8; LEN];
    let mut x = 0x9e3779b97f4a7c15u64;
    for c in data.chunks_exact_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.copy_from_slice(&x.to_le_bytes());
    }
    let mut a = tempfile::tempfile_in(dir)?;
    let mut b = tempfile::tempfile_in(dir)?;
    a.write_all(&data)?;
    b.write_all(&data)?;
    a.sync_all()?;
    b.sync_all()?;
    match dedupe_range(&a, 0, &b, 0, LEN as u64) {
        Ok(Dedupe::Unsupported) => Ok(false),
        Ok(_) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

// FICLONERANGE fallback for unpatched kernels: caller must verify ranges
// match first. Offsets and len must be recordsize-aligned (except the EOF
// tail) and both files must share blocksize, or ZFS returns EINVAL.
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

    // The test fs may or may not support these ioctls. The point is that
    // a botched struct layout would surface as EBADF/EFAULT, not one of
    // the expected outcomes.

    #[test]
    fn dedupe_ioctl() {
        let dir = tempfile::tempdir().unwrap();
        let f = std::fs::File::create_new(dir.path().join("a")).unwrap();
        f.set_len(8192).unwrap();
        if let Err(e) = dedupe_range(&f, 0, &f, 4096, 4096) {
            assert!(
                matches!(e, io::Errno::INVAL | io::Errno::XDEV),
                "unexpected errno: {e}"
            );
        }
        probe_dedupe(dir.path()).unwrap();
    }

    #[test]
    fn clone_ioctl() {
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
