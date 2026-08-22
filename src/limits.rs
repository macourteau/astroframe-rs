//! The caps.
//!
//! A per-[`Reader`](crate::Reader) parameter rather than a compile-time constant or a
//! global, because **both directions are real**. A service accepting uploads wants them
//! tighter than the defaults; a workstation tool wants the output-byte cap raised, since
//! 2³⁰ bytes is 268 megapixels at `f32` and an ordinary 3× drizzle of a 61 MP frame is
//! refused by the default.
//!
//! Two mechanisms for the same knob were rejected, both because they let one consumer's
//! choice reach another's. A **cargo feature** unifies across the whole dependency graph and
//! the union takes the loosest value, so a feature any crate in a build enables silently
//! raises the caps for a header-only consumer that never asked. A **library-global
//! initializer** is the same hazard at runtime, plus an ordering trap: a `Reader` built
//! before the initializer runs gets different limits from one built after, and neither call
//! site says so.
//!
//! The one real cost is accepted rather than argued away: a caller can set a cap high enough
//! to offer no protection. That appears in the caller's own source, which is the difference
//! between this and the two rejected mechanisms.
//!
//! ```
//! use astroframe::Limits;
//!
//! let mut limits = Limits::default();
//! limits.decoded_output_bytes = 4 << 30; // this caller produced these frames itself
//! ```

use crate::error::{Error, Result};

/// Per-`Reader` caps. Tripping one is [`Error::LimitExceeded`].
///
/// Fixed for a reader's life, with no setter: the header parse the constructor performs is
/// already subject to them, and a cap that could change afterwards would have to describe
/// which of the two settings governed the bytes already read.
///
/// Built from [`Limits::default()`] and mutated — `#[non_exhaustive]` so a later cap is an
/// addition rather than a break.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    // ---- XISF ----
    /// XISF declared XML header length, read incrementally. Default 8 MiB.
    ///
    /// This is also where the `embedded` location actually lives: for it the pixel data
    /// *is* inside the header as Base64 or Base16 text, so 8 MiB of header admits roughly
    /// 6 MB of decoded pixels — about three megapixels at `UInt16`.
    pub xml_header_bytes: u64,
    /// XISF element nesting depth. Default 64. Bounds the parser stack: Go's parser skips
    /// unknown subtrees iteratively, and a Rust parser that recurses can be made to overflow
    /// its stack, which is an abort rather than an error.
    pub xml_depth: u32,
    /// XISF total element count. Default 100 000. Bounds per-element allocation.
    pub xml_elements: u64,
    /// XISF subblock count. Default 4096.
    pub subblock_count: u32,
    /// XISF attributes per element. Default 4096.
    pub attributes_per_element: u32,
    /// XISF single attribute value length. Default 1 MiB.
    pub attribute_value_bytes: u64,
    /// A `CONTINUE` chain's **assembled** value, checked as it grows. Default 1 MiB.
    ///
    /// The only cap that bounds a value against how it was *reached* rather than how it was
    /// written. §4.2.1.2 exists to assemble a long string from short records, and XISF lets a
    /// `<Reference>` reach one continuation record many times: 2048 references to a 500 KB
    /// record assemble a gigabyte from a 553 KB header. Nothing about sharing closes that —
    /// the assembled string genuinely is that long — so a cap is the only answer.
    ///
    /// A mebibyte because a single assembled value longer than one whole attribute is already
    /// extraordinary, while the long strings the convention exists for — a WCS solution, a
    /// `HISTORY` trail — are kilobytes. FITS reaches it only in principle: 4096 cards of 70
    /// bytes bound a chain to about 287 KB, linear in the input.
    pub keyword_value_bytes: u64,
    /// XISF stored block size, as a multiple of the geometry-implied size. Default 2.
    ///
    /// A declared `attachment:pos:size` becomes an allocation, because an LZ4 block must be
    /// fully resident before it decompresses — and a compressed block may legitimately
    /// exceed its uncompressed size, so the geometry cross-check alone cannot bound it. On a
    /// seekable source the file length bounds it incidentally; on a pipe there is no file
    /// length at all.
    pub stored_block_multiple: u64,
    /// The floor beneath [`Limits::stored_block_multiple`]. Default 1 MiB.
    pub stored_block_floor_bytes: u64,
    /// The window an XISF `zstd` frame header declares. Default 8 MiB.
    ///
    /// A zstd decoder allocates that window before producing a byte, from a number the file
    /// states — the precise shape of "an allocation sized from an unvalidated declared
    /// size". zlib is discharged by its fixed ~32 KiB window and LZ4 has none, so `zstd` is
    /// the only codec carrying one.
    pub zstd_window_bytes: u64,

    // ---- FITS ----
    /// FITS header length — 2880-byte blocks read while looking for `END`. Default 8 MiB.
    pub fits_header_bytes: u64,
    /// FITS header card count. Default 4096.
    ///
    /// Real FITS headers run to tens of cards, so this is the cap that actually binds and
    /// catches a header of repeated cards long before the byte cap does. At 100 000 cards it
    /// would be pure duplication of the byte cap.
    pub fits_header_cards: u32,
    /// FITS HDUs traversed inside one `next_image()`. Default 4096.
    ///
    /// Non-image extensions and `IMAGE` extensions declaring `NAXIS = 0` alike. Over a pipe
    /// there is no file length to terminate the skip loop, which is a hang; a finite file
    /// makes it merely slow. The fuzzer cannot find this, fuzz inputs being finite by
    /// construction, which is why it needs a cap rather than a test.
    pub hdus_per_advance: u32,

    // ---- both formats ----
    /// The **file's** `width × height × channels`. Default 2³⁰.
    ///
    /// Counted whether or not `select_channel` narrowed the reader, and checked before any
    /// buffer is sized and before any sample width is known — which is the whole of what it
    /// is for, and why it is the only one of the three that can run first.
    pub total_samples: u64,
    /// The destination buffer, in bytes actually written to it. Default 2³⁰.
    ///
    /// So `select_channel` narrows it. 2³⁰ bytes is 268 megapixels of `f32`: a 3× drizzle of
    /// a 61 MP frame is refused by the default and a 2× drizzle barely fits. The default is a
    /// statement about what an unvetted file may be allowed to cost, not about which images
    /// are legitimate.
    pub decoded_output_bytes: u64,
    /// Any whole-image or row-shaped buffer this crate allocates for itself, measured at
    /// that buffer's own size. Default 2³⁰.
    ///
    /// This is what bounds tier 3, which has no destination. A `Rows`-granularity decode of a
    /// frame whose whole-image size exceeds the cap is not refused — streaming it is the
    /// point — and what the cap refuses is one allocation that reaches it.
    pub materialized_bytes: u64,
    /// A block or data unit read-and-discarded to step over it. Default 2³⁰.
    ///
    /// Bounds a *read*, not an allocation, so it does not apply where skipping is a seek: on
    /// a seekable source the cursor moves without transferring bytes and the cap is not
    /// consulted.
    pub skipped_block_bytes: u64,
    /// `next_image()` advances per source. Default 256.
    ///
    /// Inherited from an XISF decoder where multi-image is rare; FITS MEF is where it binds.
    pub images_per_source: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            xml_header_bytes: 8 << 20,
            xml_depth: 64,
            xml_elements: 100_000,
            subblock_count: 4096,
            attributes_per_element: 4096,
            attribute_value_bytes: 1 << 20,
            keyword_value_bytes: 1 << 20,
            stored_block_multiple: 2,
            stored_block_floor_bytes: 1 << 20,
            zstd_window_bytes: 8 << 20,

            fits_header_bytes: 8 << 20,
            fits_header_cards: 4096,
            hdus_per_advance: 4096,

            total_samples: 1 << 30,
            decoded_output_bytes: 1 << 30,
            materialized_bytes: 1 << 30,
            skipped_block_bytes: 1 << 30,
            images_per_source: 256,
        }
    }
}

/// Narrow a size computed in `u64` to `usize`, at the allocation site and after the relevant
/// cap has been enforced.
///
/// Checked rather than infallible, because a caller may raise a cap past what a 32-bit target
/// can address: 549 megapixels of `f32` is 2.2 GB and fits no 32-bit `usize`. A `u64` that
/// does not fit is [`Error::LimitExceeded`], the same class the cap itself raises. The
/// defaults keep it unreachable, which is what their sub-`i32::MAX` values buy.
pub(crate) fn narrow(bytes: u64, what: &str) -> Result<usize> {
    usize::try_from(bytes).map_err(|_| {
        Error::limit(format!(
            "{what}: {bytes} bytes does not fit this target's usize"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The design document is the single normative home of the caps, and these are the values
    /// its table states. Pinning them is not ceremony: a default is a decision about what an
    /// unattended process should absorb from a file it has not vetted, and a silent change to
    /// one is a security-relevant change that no other test would notice — every cap test
    /// asserts that a cap *trips*, which stays true at any value.
    #[test]
    fn the_shipped_defaults_are_the_ones_the_design_states() {
        let d = Limits::default();

        // XISF
        assert_eq!(d.xml_header_bytes, 8 << 20, "XML header length: 8 MiB");
        assert_eq!(d.xml_depth, 64, "element nesting depth");
        assert_eq!(d.xml_elements, 100_000, "total element count");
        assert_eq!(d.subblock_count, 4096, "subblock count");
        assert_eq!(d.attributes_per_element, 4096, "attributes per element");
        assert_eq!(
            d.attribute_value_bytes,
            1 << 20,
            "attribute value length: 1 MiB"
        );
        assert_eq!(
            d.keyword_value_bytes,
            1 << 20,
            "assembled keyword value: 1 MiB"
        );
        assert_eq!(
            d.stored_block_multiple, 2,
            "stored block: geometry-implied size x 2"
        );
        assert_eq!(
            d.stored_block_floor_bytes,
            1 << 20,
            "stored block floor: 1 MiB"
        );
        assert_eq!(d.zstd_window_bytes, 8 << 20, "zstd decoder window: 8 MiB");

        // FITS
        assert_eq!(d.fits_header_bytes, 8 << 20, "FITS header length: 8 MiB");
        assert_eq!(d.fits_header_cards, 4096, "header card count");
        assert_eq!(d.hdus_per_advance, 4096, "HDUs traversed per advance");

        // Both formats
        assert_eq!(d.total_samples, 1 << 30, "total samples");
        assert_eq!(d.decoded_output_bytes, 1 << 30, "decoded output bytes");
        assert_eq!(d.materialized_bytes, 1 << 30, "materialized bytes");
        assert_eq!(d.skipped_block_bytes, 1 << 30, "skipped block bytes");
        assert_eq!(d.images_per_source, 256, "images per source");
    }

    /// Three of the format-neutral caps are chosen to sit deliberately below `i32::MAX`. That is what keeps the checked `u64`-to-`usize`
    /// narrowing unreachable under the defaults, on every target this crate supports.
    #[test]
    fn the_format_neutral_defaults_stay_below_i32_max() {
        let d = Limits::default();
        let ceiling = i32::MAX as u64;
        for (name, value) in [
            ("total_samples", d.total_samples),
            ("decoded_output_bytes", d.decoded_output_bytes),
            ("materialized_bytes", d.materialized_bytes),
            ("skipped_block_bytes", d.skipped_block_bytes),
        ] {
            assert!(value < ceiling, "{name} = {value} must stay below i32::MAX");
        }
    }

    /// The narrowing is checked rather than infallible, and refuses with the same class the
    /// cap itself raises.
    #[test]
    fn narrowing_a_size_beyond_usize_is_limit_exceeded() {
        assert!(narrow(1024, "probe").is_ok());
        if usize::BITS < 64 {
            let err = narrow(u64::MAX, "probe").expect_err("cannot fit a 32-bit usize");
            assert!(matches!(err, Error::LimitExceeded(_)));
        }
    }
}
