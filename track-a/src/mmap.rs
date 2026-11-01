//! Memory settings and reserved mappings for the process, without the `libc` crate.

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

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

const PROT_READ_WRITE: i32 = 1 | 2;
/// MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE.
#[cfg(target_os = "linux")]
const ANON_NORESERVE: i32 = 0x02 | 0x20 | 0x4000;
#[cfg(not(target_os = "linux"))]
const ANON_NORESERVE: i32 = 0x02 | 0x1000 | 0x40;

/// Reserves `len` bytes of zeroed memory. Nothing is committed up front: a page is
/// charged only when it is first written.
pub fn reserve(len: usize) -> *mut u8 {
    // SAFETY: a fresh anonymous mapping with no address hint and no file; it overlaps
    // nothing else in the process.
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ_WRITE,
            ANON_NORESERVE,
            -1,
            0,
        )
    };
    assert!(ptr as isize != -1, "mmap of {len} bytes failed");
    ptr
}

/// Returns a mapping made by `reserve` to the system, with all its pages.
///
/// # Safety
/// `ptr` and `len` are exactly those of one `reserve` call, and nothing reads or
/// writes the memory afterwards.
pub unsafe fn release(ptr: *mut u8, len: usize) {
    // SAFETY: by the contract above, the range is one whole mapping of ours that is
    // no longer used.
    unsafe { munmap(ptr, len) };
}
