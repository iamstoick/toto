//! Stack measurement (ported from Spike 1b, reference only): pattern
//! fill at spawn, upward scan after the task finishes. The guard page is
//! never touched by either operation.
//!
//! The `unsafe` here is contained and justified: the mapping is owned by
//! the (dead or not-yet-started) task's stack, the range is inside the
//! writable region by construction, and no task runs during fill or scan.

const PATTERN: u64 = 0xAA_AA_AA_AA_AA_AA_AA_AA;

#[cfg(unix)]
pub fn page_size() -> usize {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    assert!(ps.is_power_of_two(), "page size must be a power of two");
    ps
}

/// Windows fallback: `GetSystemInfo` from kernel32 (always linked;
/// no new dependency). `SYSTEM_INFO` is 48 bytes on 64-bit Windows
/// with `dwPageSize` as the second DWORD (offset 4) — stable ABI,
/// documented on MSDN. Only the page-size field is read.
#[cfg(windows)]
pub fn page_size() -> usize {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemInfo(info: *mut [u8; 48]);
    }
    let mut buf = [0u8; 48];
    unsafe { GetSystemInfo(&mut buf) };
    let ps = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    assert!(ps.is_power_of_two(), "page size must be a power of two");
    ps
}

/// First writable byte above the guard page. `limit` includes the guard
/// page (corosensei `Stack` contract); the guard is one page.
pub fn writable_start(limit: usize) -> usize {
    limit + page_size()
}

/// Fill `[start, base)` with the pattern. The task must not have run yet.
///
/// # Safety
/// `[start, base)` must be a writable mapping owned by the caller.
pub unsafe fn fill_pattern(start: usize, base: usize) {
    assert!(start <= base);
    let words = (base - start) / 8;
    let ptr = start as *mut u64;
    for i in 0..words {
        unsafe { ptr.add(i).write(PATTERN) };
    }
    let tail = start + words * 8;
    for i in 0..(base - tail) {
        unsafe { (tail as *mut u8).add(i).write(0xAA) };
    }
}

/// Bytes of stack used: `base - first_modified_word`.
///
/// # Safety
/// Same mapping requirements as `fill_pattern`; the owning task must be
/// finished (its stack is dead) so nothing mutates under the scan.
pub unsafe fn scan_high_water(start: usize, base: usize) -> usize {
    if start >= base {
        return 0;
    }
    let words = (base - start) / 8;
    let ptr = start as *const u64;
    for i in 0..words {
        if unsafe { ptr.add(i).read() } != PATTERN {
            return base - (start + i * 8);
        }
    }
    base - (start + words * 8)
}
