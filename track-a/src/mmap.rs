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
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
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

/// Maps every page under `part` (a piece of a `map` result) in one system call
/// instead of one page fault per few pages. Linux 5.14 and later; elsewhere, or if
/// refused, the pages simply fault in as they are read.
pub fn populate(part: &[u8]) {
    #[cfg(target_os = "linux")]
    {
        const MADV_POPULATE_READ: i32 = 22;
        let start = part.as_ptr() as usize & !4095;
        let len = part.as_ptr() as usize + part.len() - start;
        // SAFETY: the range lies inside a live read-only file mapping (rounded down to
        // its page, which belongs to the same mapping), and populating changes no data.
        unsafe { madvise(start as *mut u8, len, MADV_POPULATE_READ) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = part;
}

/// Opts this process out of transparent huge pages. With THP on, the first byte a
/// thread allocates in its own malloc arena (or touches in a sparse table) pulls in a
/// whole 2 MB page, and the run's peak memory is scored. Linux only; a no-op elsewhere.
pub fn small_pages() {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn prctl(option: i32, ...) -> i32;
        }
        const PR_SET_THP_DISABLE: i32 = 41;
        // SAFETY: prctl with PR_SET_THP_DISABLE takes one integer flag and only changes
        // how this process's future page faults are served.
        unsafe { prctl(PR_SET_THP_DISABLE, 1u64, 0u64, 0u64, 0u64) };
    }
}
