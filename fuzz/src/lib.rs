//! The fuzz oracle, and the counting allocator it needs.
//!
//! **The oracle is not merely "it didn't crash".** Each target asserts that the call returns
//! rather than panicking or aborting, that it terminates, and that total allocation stays
//! under a bound. The caps are deliberately *not* that bound: summing them would admit a
//! gigabyte of allocation from a twenty-byte input and assert almost nothing.
//!
//! Measuring total allocation needs a counting global allocator, which needs `unsafe impl`.
//! It lives **here, in the fuzz crate, not in the library**, so the library's
//! `#![forbid(unsafe_code)]` is unaffected.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use astroframe::{Limits, Reader};

/// Where the general allocation bound comes from.
///
/// On a seekable source the geometry-versus-length check already bounds an uncompressed image
/// by the bytes the input contains, so the widest legitimate expansion left on every target
/// but `decode_alloc` is a `UInt8` frame normalized to `f32` — exactly 4×, with the native
/// samples beneath it bounded by the input length. **Eight is that figure with one doubling
/// of slack.** The general bound carries no room for decompression or for a large image
/// because the target that needs that room, `decode_alloc`, carries its own geometry-implied
/// term instead.
///
/// One interaction follows from the derivation and is stated so it is not met as a surprise:
/// the 4× ceiling rests on the input length bounding the stored image, which a **compressed**
/// block breaks, its decompressed size being bounded by the caps rather than by the input.
/// Where that bites, the knob is the [`limits`] this crate passes — **not** this number.
///
/// # Changing it is not tuning, and the conditions run one way only
///
/// - **A failure at the current value is presumed to be a bug in the allocation, not a bad
///   bound.** "The current value", not a figure: a stale number here would misdescribe the
///   bound in force, which is the one spelling a record like this cannot afford. That presumption is most of what the number is for. A failing run is evidence about the code
///   until someone shows otherwise, and the burden of showing otherwise sits with whoever
///   wants the bound raised.
/// - **Raising it requires naming the buffer that allocated and showing why that allocation
///   is legitimate.** A general argument — that the bound seemed too tight, that some
///   headroom is prudent, that the failure looked unrelated — is not a justification. Name
///   the buffer, or the bound stands.
/// - **The justification is recorded in the design document**, beside the paragraph that
///   fixes this number. Not in a commit message, where the next person to reach for it will
///   not find it, and where a sequence of raises leaves no single place that shows it was a
///   sequence.
///
/// The hazard is not one wrong number. It is a bound that ratchets loose one
/// individually-reasonable step at a time — each raise justified on its own, none of them
/// looking like the moment the oracle stopped asserting anything, and the requirement to
/// justify being exactly what makes every step feel principled.
///
/// **The bound is 32, not the 8 a pixel-path derivation gives.** Eight
/// is derived from the pixel path alone — a `UInt8` frame normalized to `f32` is exactly 4×,
/// doubled for slack — and header parsing was never in it. A DOM's cost is per *element*, not
/// per byte: `<Reference ref="k"/>` is twenty source bytes and about five hundred allocated
/// once a node, its attribute list, the metadata it materializes and the amortized growth of
/// the vectors holding them are counted. That is ~25× on header-dense input, structurally,
/// with nothing left to remove. Thirty-two is that figure with slack.
///
/// The three conditions above are met rather than waived. The presumption that a failure is a
/// bug in the allocation was honoured first and at length: interning the XML strings into one
/// arena, packing a keyword's three texts into one allocation, not building occurrences past
/// `Images per source`, sharing a keyword reached through many references, and sharing root
/// `Metadata` rather than cloning it per image together took the worst shape from 2.08 GB to
/// 24 MB. The buffers are named in § Fuzzing. And the raise still catches every defect this
/// crate has actually had: those shapes ran at 2080×, 262× and 127× input length, all of them
/// an order of magnitude above the new bound.
pub const ALLOC_MULTIPLE: usize = 32;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

/// A `System` allocator that keeps a running total of bytes handed out.
pub struct Counting;

// SAFETY: every method forwards to `System`, which upholds the `GlobalAlloc` contract; the
// only addition is an atomic counter, which cannot affect the returned pointers.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    /// Counts the **growth**, not the whole new block.
    ///
    /// A `Vec` doubling from 4 MB to 8 MB is charged 4 MB here, and the 4 MB it already held
    /// was charged when it was allocated — so the running total is what the program has ever
    /// asked for, counted once per byte. Charging `new_size` would double-count every byte
    /// carried across a growth and make the oracle's figure depend on how many times a buffer
    /// happened to be resized rather than on how much it holds.
    ///
    /// § Fuzzing describes the same measurement as one where "every reallocation copy counted".
    /// That sentence is loose about this accounting; the conclusion it draws is unaffected,
    /// because the arena's doubling was charged the same way under both readings — a linear
    /// pass over the header's `<` count still replaces log-many growth charges with one.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Reset the counter and return a token that reads the total afterwards.
pub fn start_counting() -> AllocationRun {
    ALLOCATED.store(0, Ordering::Relaxed);
    AllocationRun
}

/// Reads the running allocation total.
pub struct AllocationRun;

impl AllocationRun {
    /// Total bytes allocated since [`start_counting`].
    pub fn total(&self) -> usize {
        ALLOCATED.load(Ordering::Relaxed)
    }
}

/// The `Limits` every target constructs its `Reader` with — **far tighter than the shipped
/// defaults**.
///
/// Small limits are what a fuzz target should use whatever the oracle asserts: many fast
/// iterations exploring parser state space find more than a few large ones exercising
/// allocation volume, and large-geometry behaviour belongs to the caps tests and the local
/// corpus rather than to the fuzzer. Without them a twenty-byte input declaring 2²⁸ samples
/// allocates a gigabyte and *passes*, which libFuzzer's default RSS limit and any realistic
/// corpus throughput make unworkable.
///
/// This is § The caps being tightened by its first caller rather than a fuzz-only constant:
/// the fuzz crate passes a `Limits` like anyone else and the shipped defaults are unchanged.
///
/// **Only these two move.** The pair is one number in two units — 2¹⁸ samples at `f32` is
/// exactly 2²⁰ bytes — so neither trips before the other on normalized output. The other
/// fuzz-reachable buffers follow the same geometry rather than needing their own tightening:
/// at 2¹⁸ samples the whole-image scratch is at most 2 MiB for the widest sample type and the
/// stored block at most 4 MiB, that cap being the geometry-implied size × 2. Both sit under
/// the header-cap term on their own.
pub fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.total_samples = 1 << 18;
    limits.decoded_output_bytes = 1 << 20;
    limits
}

/// The general allocation bound: `ALLOC_MULTIPLE × input_length + header_cap`.
///
/// The header-cap term is there because a twenty-byte input may legitimately declare a header
/// the reader grows toward.
pub fn bound(input_len: usize) -> usize {
    ALLOC_MULTIPLE
        .saturating_mul(input_len)
        .saturating_add(limits().xml_header_bytes as usize)
}

/// The ceiling on `decode_alloc`'s geometry-implied term — the design document's words for
/// it are **"the geometry-implied size"**, singular, "on top of" the general [`bound`]. It
/// stands for the widest single legitimate expansion a compressed block's declared-versus-
/// stored gap can produce, not a sum across every image occurrence an input can enumerate:
/// [`bound`] already scales with input length, and more images cost more input bytes to
/// declare, so that scaling is already covered there. Without an overall ceiling a
/// multi-image input inflates this term without limit and the oracle stops asserting
/// anything.
pub const IMPLIED_CEILING: usize = 1 << 24;

/// Fold one image occurrence's declared geometry onto the running geometry-implied total,
/// clamped overall at [`IMPLIED_CEILING`] rather than per occurrence.
///
/// `width`, `height` and `channels` are declared, attacker-controlled `u32`s, so the sample
/// count is computed in `u64` with `saturating_mul` rather than `*`. Three `u32`s reach 2^96
/// and `u64` holds 2^64, so `*` panics under the debug assertions `cargo-fuzz` builds with —
/// an oracle that aborts on a hostile input reports a crash the library did not have.
/// `Geometry::total_samples` saturates for the same reason and one more: on the library side
/// a wrap would also walk the declaration through the total-samples cap. This oracle once
/// cited that function as already safe, when it was the code that made it safe.
pub fn accumulate_implied(
    total: usize,
    width: u32,
    height: u32,
    channels: u32,
    sample_bytes: u32,
) -> usize {
    let samples = u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(u64::from(channels));
    let geometry_bytes = samples.saturating_mul(u64::from(sample_bytes.max(4)));
    total
        .saturating_add(geometry_bytes.min(IMPLIED_CEILING as u64) as usize)
        .min(IMPLIED_CEILING)
}

/// Assert the oracle for a run that has finished.
pub fn assert_within(run: &AllocationRun, bound: usize, target: &str, input_len: usize) {
    let total = run.total();
    assert!(
        total <= bound,
        "{target}: allocated {total} bytes from a {input_len}-byte input, above the {bound} \
         byte bound. Presume this is a bug in the allocation, not a bad bound: name the \
         buffer that allocated and show why it is legitimate before touching ALLOC_MULTIPLE."
    );
}

/// Walk every image of a seekable source, header-only.
///
/// Errors are outcomes, not failures — the contract being that malformed input produces an
/// `Err` rather than a panic, a hang, or unbounded allocation.
pub fn walk_headers(data: &[u8]) {
    let Ok(mut reader) = Reader::seekable_with_limits(std::io::Cursor::new(data), limits()) else {
        return;
    };
    let mut advances = 0;
    while advances < 1024 {
        match reader.next_image() {
            Ok(true) => {}
            Ok(false) | Err(_) => return,
        }
        let _ = reader.header();
        advances += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single image at the per-occurrence cap must reach `IMPLIED_CEILING` on its own — the
    /// ceiling has to be reachable by one occurrence, not just an accumulation artifact.
    #[test]
    fn one_image_can_reach_the_ceiling() {
        let total = accumulate_implied(0, 4096, 4096, 4, 4);
        assert_eq!(total, IMPLIED_CEILING);
    }

    /// Before this fix, `decode_alloc` added `min(1 << 24)` per image occurrence with no
    /// overall ceiling, so a multi-image input pushed the term past any fixed bound. Five
    /// occurrences each at the per-image cap must still clamp to `IMPLIED_CEILING` overall,
    /// not `5 * IMPLIED_CEILING`.
    #[test]
    fn multi_image_accumulation_is_capped_overall_not_per_image() {
        let mut total = 0usize;
        for _ in 0..5 {
            total = accumulate_implied(total, 4096, 4096, 4, 4);
        }
        assert_eq!(total, IMPLIED_CEILING);
    }

    /// A geometry small enough that neither the per-call minimum nor the overall ceiling
    /// binds must still accumulate exactly, so the ceiling is not silently swallowing the
    /// legitimate case it exists to bound. `sample_bytes` is floored at 4 (the normalized
    /// `f32` width), matching the general bound's own widest-legitimate-expansion reasoning.
    #[test]
    fn small_geometry_accumulates_exactly_below_the_ceiling() {
        let total = accumulate_implied(0, 10, 10, 1, 2);
        assert_eq!(total, 10 * 10 * 4);
        let total = accumulate_implied(total, 10, 10, 1, 2);
        assert_eq!(total, 2 * 10 * 10 * 4);
    }

    /// `width * height * channels` overflows `u32` arithmetic at declared values well within
    /// what a hostile header can spell, and must saturate rather than wrap or panic.
    #[test]
    fn a_hostile_declaration_does_not_overflow() {
        let total = accumulate_implied(0, u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        assert_eq!(total, IMPLIED_CEILING);
    }
}
