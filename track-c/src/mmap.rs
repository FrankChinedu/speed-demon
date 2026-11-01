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

/// Opts this process out of transparent huge pages. With THP on, the first byte a
/// thread allocates in its own malloc arena (or touches in a sparse table) pulls in a
/// whole 2 MB page, and the run's peak memory is scored. Linux only; a no-op elsewhere.
pub fn small_pages() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_ARENA_MAX: i32 = -8;
        // EXPERIMENT: huge pages left on (fewer TLB misses on the random table lookups),
        // with one malloc arena so threads do not each touch a fresh 2 MB arena page.
        // SAFETY: mallopt only sets an allocator parameter, before any thread starts.
        unsafe { mallopt(M_ARENA_MAX, 1) };
    }
}
