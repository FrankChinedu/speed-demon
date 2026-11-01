//! Read-only memory map of an input file, without the `libc` crate.
//!
//! Mapping instead of reading matters for the M axis: the harness pre-warms /data
//! into the host's page cache, so mapped pages are not charged to our cgroup,
//! while a `Vec` copy of the file would be.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2; // same value on Linux and macOS

/// Maps the whole file for the life of the process (it is never unmapped).
pub fn map(path: &Path) -> &'static [u8] {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let len = file.metadata().expect("stat input").len() as usize;
    if len == 0 {
        return &[];
    }
    // SAFETY: a fresh read-only private mapping of `len` bytes of an open file. The
    // input is read-only for the whole run, so the bytes never change under us.
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ,
            MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    assert!(ptr as isize != -1, "mmap {} failed", path.display());
    // SAFETY: the mapping is valid for `len` bytes and lives until the process exits.
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// Makes every thread allocate from glibc's one main arena instead of an arena of its
/// own. With transparent huge pages on, a thread arena that grows past 2 MB can be
/// backed by huge pages charged whole. The workers allocate a few buffers per chunk,
/// so sharing one arena costs nothing measurable. Call before any thread starts. A
/// no-op off Linux.
pub fn one_arena() {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_ARENA_MAX: i32 = -8;
        // SAFETY: mallopt only sets an allocator tuning parameter, and runs before
        // any thread is started.
        unsafe { mallopt(M_ARENA_MAX, 1) };
    }
}
