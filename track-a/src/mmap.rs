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
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 2; // same value on Linux and macOS
const MAP_ANONYMOUS: i32 = if cfg!(target_os = "linux") {
    0x20
} else {
    0x1000
};
const MADV_DONTNEED: i32 = 4;
const PAGE: usize = 4096;

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

/// `len` bytes of zeroed, page-aligned anonymous memory for the life of the process.
/// Pages are charged only once written.
pub fn anon(len: usize) -> *mut u8 {
    // SAFETY: a fresh private anonymous mapping; it aliases nothing.
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len.max(1),
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(ptr as isize != -1, "mmap of {len} bytes failed");
    ptr
}

/// Gives the pages under `mem` back to the system. `mem` must start on a page
/// boundary inside memory from `anon`, and own the rest of its last page too.
pub fn release(mem: &'static mut [u32]) {
    let len = (mem.len() * 4).next_multiple_of(PAGE);
    // SAFETY: `mem` is consumed, so nothing reads these pages again; DONTNEED on
    // private anonymous memory only drops them (they would read back as zero).
    unsafe { madvise(mem.as_mut_ptr().cast(), len, MADV_DONTNEED) };
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
