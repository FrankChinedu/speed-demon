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
/// Release granularity: a transparent huge page. Dropping part of a 2 MB page only
/// splits it, and the kernel keeps the whole page charged until memory runs short.
const PAGE: usize = 2 << 20;

/// `n` zeroed atomics in a private anonymous mapping that lives until exit. Pages
/// are charged once written, and `release` hands them back early.
pub fn zeroed_atomics(n: usize) -> &'static [std::sync::atomic::AtomicU64] {
    let len = (n * 8).max(1);
    // SAFETY: a fresh private anonymous mapping, aliasing nothing.
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(ptr as isize != -1, "mmap of {len} bytes failed");
    // SAFETY: the mapping is zeroed, page-aligned and `len` bytes long, and an
    // all-zero AtomicU64 is a valid value.
    unsafe { std::slice::from_raw_parts(ptr.cast(), n) }
}

/// Gives back the whole (huge) pages inside `words[from..to]` of a `zeroed_atomics`
/// mapping. The caller never reads that range again (it would read zeros).
pub fn release(words: &[std::sync::atomic::AtomicU64], from: usize, to: usize) {
    let base = words.as_ptr() as usize;
    let start = (base + from * 8).next_multiple_of(PAGE);
    let end = (base + to * 8) & !(PAGE - 1);
    if start < end {
        // SAFETY: the range lies in the mapping, and DONTNEED on private anonymous
        // memory only drops pages, which nothing reads again.
        unsafe { madvise(start as *mut u8, end - start, MADV_DONTNEED) };
    }
}

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
