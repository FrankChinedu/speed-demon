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
}
