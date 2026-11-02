//! What a tool does to its own process on a desktop: a heap that zeroes what it
//! frees and stays out of swap, and no core dump. Only the binaries compile this
//! file, by `#[path]`. It is not in the library on purpose: the library is what a
//! mobile build shares, and a phone's allocator and process limits are not ours
//! to set. `tests/platform.rs` holds the library to that.

// Each binary uses its own part of this file.
#![allow(dead_code)]

use anyhow::bail;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

/// What must lock for a tool to call itself locked: age's chunk, the tools' own
/// buffer, and room for both to be copied.
const PROBE: usize = 256 * 1024;

static LOCKING: AtomicBool = AtomicBool::new(false);
static PAGE: AtomicUsize = AtomicUsize::new(4096);
static FAILED: AtomicUsize = AtomicUsize::new(0);
static LAST_ERROR: AtomicI32 = AtomicI32::new(0);

/// An allocator over `A` that overwrites every block as it is freed, whoever
/// asked for it, and locks every block once [`lock_heap`] has run. So the `Vec`s
/// inside the `age` crate, which no `Zeroizing` reaches, are wiped too.
///
/// `realloc` is the trait's default on purpose: allocate, copy, free through this
/// type. Forwarded to `A`, a block that moved would leave its old bytes unwiped.
pub struct Zeroing<A = System>(pub A);

/// The whole pages under a block.
fn pages(ptr: *mut u8, size: usize) -> (*const libc::c_void, usize) {
    let page = PAGE.load(Ordering::Relaxed);
    let start = ptr as usize & !(page - 1);
    let end = (ptr as usize + size).div_ceil(page) * page;
    (start as *const libc::c_void, end - start)
}

/// # Safety
/// `ptr` is null or a live block of `size` bytes.
unsafe fn lock_block(ptr: *mut u8, size: usize) {
    if ptr.is_null() || !LOCKING.load(Ordering::Relaxed) {
        return;
    }
    let (start, length) = pages(ptr, size);
    if libc::mlock(start, length) != 0 {
        LAST_ERROR.store(std::io::Error::last_os_error().raw_os_error().unwrap_or(0), Ordering::Relaxed);
        FAILED.fetch_add(1, Ordering::Relaxed);
    }
}

/// macOS counts a page's locks, and past 65535 `mlock` says `ENOMEM`, as a real
/// limit would: a long-running tool reusing one page gets there. So each lock
/// there has its unlock, and a page shared with a live block stays locked.
///
///
/// # Safety
/// `ptr` is a live block of `size` bytes.
#[cfg(target_vendor = "apple")]
unsafe fn unlock_block(ptr: *mut u8, size: usize) {
    if LOCKING.load(Ordering::Relaxed) {
        let (start, length) = pages(ptr, size);
        libc::munlock(start, length);
    }
}

/// Linux keeps no count: one `munlock` would unlock a neighbour on a shared page.
/// So there only the pages wholly inside the block are unlocked, which no other
/// block can be on. Left locked, a large value's pages would hold the quota after
/// the value had gone, and a failed lock would outlive the request that caused it.
///
/// # Safety
/// `ptr` is a live block of `size` bytes, already wiped.
#[cfg(not(target_vendor = "apple"))]
unsafe fn unlock_block(ptr: *mut u8, size: usize) {
    if LOCKING.load(Ordering::Relaxed) {
        let page = PAGE.load(Ordering::Relaxed);
        let start = (ptr as usize).div_ceil(page) * page;
        let end = (ptr as usize + size) & !(page - 1);
        if end > start {
            libc::munlock(start as *const libc::c_void, end - start);
        }
    }
}

// SAFETY: every method hands the caller's layout to `A` unchanged. Zeroing writes
// only inside the block being freed, and allocates nothing.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Zeroing<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let block = self.0.alloc(layout);
        lock_block(block, layout.size());
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let block = self.0.alloc_zeroed(layout);
        lock_block(block, layout.size());
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        use zeroize::Zeroize;
        // Volatile, inside zeroize: a plain write before a free is a dead store,
        // and the optimizer may drop it.
        std::slice::from_raw_parts_mut(block, layout.size()).zeroize();
        unlock_block(block, layout.size());
        self.0.dealloc(block, layout);
    }
}

/// From here on every heap block is locked out of swap, and this tries it. `Err`
/// says why it cannot: `RLIMIT_MEMLOCK`, usually. Why not `mlockall`: on macOS,
/// where these tools run, it is `ENOSYS`; and on Linux `MCL_FUTURE` counts each
/// new mapping against that limit, so a thread's stack can fail to map and take
/// the process down. A failed `mlock` costs nothing but the lock.
pub fn lock_heap() -> Result<(), String> {
    // SAFETY: sysconf only reads.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page > 0 {
        PAGE.store(page as usize, Ordering::Relaxed);
    }
    LOCKING.store(true, Ordering::Relaxed);
    let before = lock_failures();
    std::hint::black_box(vec![1u8; PROBE]);
    if lock_failures() != before {
        return Err(std::io::Error::from_raw_os_error(LAST_ERROR.load(Ordering::Relaxed)).to_string());
    }
    Ok(())
}

/// How many blocks could not be locked, so far.
pub fn lock_failures() -> usize {
    FAILED.load(Ordering::Relaxed)
}

/// What [`lock_heap`] said, handled. `require` stops the tool: `vault-approve` at
/// its work and `vault-encrypt` always pass it, and `recover` only when told to,
/// since a recovery on a bad day must still work. Otherwise the owner hears about
/// it, as a value handled now may reach swap.
pub fn require_or_warn(locked: Result<(), String>, require: bool) -> anyhow::Result<()> {
    if let Err(reason) = locked {
        if require {
            bail!("memory is not locked, so this will not handle a value: {reason}. Raise the limit on locked memory (ulimit -l)");
        }
        eprintln!("warning: memory is NOT locked, so a value may reach swap: {reason}. --require-memory-lock makes this fatal");
    }
    Ok(())
}

/// Whether one piece of work ran on locked memory. Begin it before the first block
/// that holds a value is asked for: every block the work asks for after that is
/// tried at that moment, so one that fails is counted here, and a block that failed
/// long ago, for something else, is not. A count kept since startup would turn one
/// failure into a refusal of everything after it, until a restart, for a value that
/// was never at risk. Blocks from other threads count too: that only ever refuses.
pub struct LockWatch {
    failures_before: usize,
}

impl LockWatch {
    pub fn begin() -> Self {
        LockWatch { failures_before: lock_failures() }
    }

    /// Whether every block since [`LockWatch::begin`] locked. For a long-running
    /// tool the probe at the start says nothing about the heap of an hour later.
    pub fn check(&self, require: bool) -> anyhow::Result<()> {
        let failures = lock_failures() - self.failures_before;
        if failures == 0 {
            return Ok(());
        }
        require_or_warn(Err(format!("{failures} heap blocks did not lock")), require)
    }
}

/// A core dump of a process holding a value would be a value on disk.
pub fn no_core_dumps() -> anyhow::Result<()> {
    let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `none` outlives the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) } != 0 {
        bail!("could not turn core dumps off: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// For a tool that holds a value itself: [`no_core_dumps`], and on Linux, where
/// the kernel does not apply `RLIMIT_CORE` to a core piped to a handler such as
/// systemd-coredump, undumpable. That also shuts `/proc/<pid>/mem` and ptrace to
/// this user's other processes. macOS has no such handler.
pub fn undumpable() -> anyhow::Result<()> {
    no_core_dumps()?;
    #[cfg(target_os = "linux")]
    if !keep_dumpable() {
        // SAFETY: PR_SET_DUMPABLE takes only the integer.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } != 0 {
            bail!("could not make this process undumpable: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The test suite's way in to this process's memory: see `memory-scan-test`.
#[cfg(all(target_os = "linux", feature = "memory-scan-test"))]
fn keep_dumpable() -> bool {
    std::env::var_os("VAULT_TEST_KEEP_DUMPABLE").is_some()
}

#[cfg(all(target_os = "linux", not(feature = "memory-scan-test")))]
fn keep_dumpable() -> bool {
    false
}
