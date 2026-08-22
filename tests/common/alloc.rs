//! A counting global allocator, for the tests that assert an allocation *does not happen*.
//!
//! Not declared in `common/mod.rs`: a `#[global_allocator]` is process-wide for the binary
//! that links it, and most test binaries have no business swapping theirs. The two that need
//! it pull this file in directly:
//!
//! ```text
//! #[path = "common/alloc.rs"]
//! mod alloc;
//! ```
//!
//! The library is `#![forbid(unsafe_code)]` and stays so — an integration test crate is not
//! the library, which is the same carve-out the fuzz crate's oracle takes.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// Bytes handed out and not yet returned, and the high-water mark of that figure.
///
/// Signed because a block allocated *before* [`reset`] and freed inside the measured region
/// takes the live figure below its starting point; that is not an error, and clamping it at
/// zero would hide it. The peak is what the § Streaming table states, and it is the only
/// measure that can tell a decode holding one subblock at a time from one holding all of
/// them: their cumulative totals are identical.
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

fn grew(bytes: usize) {
    let live = LIVE.fetch_add(bytes as isize, Ordering::Relaxed) + bytes as isize;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

/// `System`, plus a running total of the bytes handed out.
pub struct Counting;

// SAFETY: every method forwards to `System`, which upholds the `GlobalAlloc` contract. The
// only addition is an atomic counter, which cannot affect the pointers returned.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        grew(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        grew(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
}

/// Zero the counters. Call immediately before the measured region.
pub fn reset() {
    ALLOCATED.store(0, Ordering::Relaxed);
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
}

/// Bytes allocated since [`reset`], freed or not.
pub fn total() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

/// The most bytes live at once since [`reset`].
pub fn peak() -> usize {
    usize::try_from(PEAK.load(Ordering::Relaxed)).unwrap_or(0)
}
