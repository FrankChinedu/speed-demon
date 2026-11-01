//! Memory settings for the process, without the `libc` crate.

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
    // Pin glibc's mmap threshold at 64 KB. Left dynamic, it rises to the size of the
    // first mapped block freed, and later record blocks then come from the malloc
    // arenas, which keep freed memory instead of returning it. Pinned, every block of
    // 64 KB or more is its own mapping, returned the moment it is freed.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_MMAP_THRESHOLD: i32 = -3;
        // SAFETY: mallopt only sets an allocator tuning parameter, and runs before
        // any thread is started.
        unsafe { mallopt(M_MMAP_THRESHOLD, 64 << 10) };
    }
}
