//! Criterion *Every cap has a test that trips it*, for every cap a FITS fixture can reach.
//!
//! The FITS caps are graded over `Reader::sequential` — a non-seekable source with no known
//! length, which is the case that has no file size to fall back on and where the caps are the
//! floor rather than a backstop. The two places the source mode is itself the thing under
//! test say so at the test.

mod common;

use std::io::Cursor;

use astroframe::{Limits, Reader, Sequential};

use common::{
    Hdu, STORED_2X2, assert_same_bits, empty_primary, file, kind, table_extension, unsigned_2x2,
    unsigned_2x2_expected, unsigned_2x2_extension, unsigned_2x2x3,
};

// ------------------------------------------------------------------ helpers

type SeqReader = Reader<Sequential<Cursor<Vec<u8>>>>;

fn sequential(bytes: Vec<u8>, limits: Limits) -> astroframe::Result<SeqReader> {
    Reader::sequential_with_limits(Cursor::new(bytes), limits)
}

// ------------------------------------------------------------------ fixtures

/// A decodable 4x4 frame: 16 samples, one 8-byte row.
fn unsigned_4x4() -> Hdu {
    let stored: Vec<i16> = (0..16).map(|i| i as i16 * 1000 - 8000).collect();
    Hdu::primary()
        .image_2d(16, 4, 4)
        .unsigned_convention(16)
        .data_i16(&stored)
}

/// A primary whose header runs to three 2880-byte blocks: 7 structural cards, `extra`
/// commentary cards and `END`, at 36 cards to the block.
fn padded_primary(extra: usize) -> Hdu {
    let mut hdu = Hdu::primary().image_2d(16, 2, 2).unsigned_convention(16);
    for i in 0..extra {
        hdu = hdu.commentary("COMMENT", &format!("padding card {i}"));
    }
    hdu.data_i16(&STORED_2X2)
}

// ------------------------------------------------------------------ the FITS caps

/// Criterion *Every cap has a test that trips it* — `fits_header_bytes`.
///
/// FITS has no declared header length to bound the search for `END`, and over a pipe there is
/// no file length either, so this cap is the only thing standing between a header of repeated
/// blocks and a keyword list that grows until the process dies.
#[test]
fn fits_header_bytes_cap_trips() {
    let bytes = file(&[padded_primary(80)]);
    let mut limits = Limits::default();
    limits.fits_header_bytes = 2 * 2880;
    let err = sequential(bytes.clone(), limits).expect_err("the byte cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("FITS header length"), "{err}");

    // The same header parses once the cap admits its third block, which is what shows the cap
    // and not the fixture is what refused it.
    let mut limits = Limits::default();
    limits.fits_header_bytes = 3 * 2880;
    let mut reader = sequential(bytes, limits).expect("three blocks fit the raised cap");
    assert!(reader.next_image().expect("advancing succeeds"));
}

/// Criterion *Every cap has a test that trips it* — `fits_header_cards`.
///
/// The card cap is the one that actually binds: real headers run to tens of cards, so a header
/// of repeated cards trips this long before the byte cap.
#[test]
fn fits_header_cards_cap_trips() {
    let bytes = file(&[padded_primary(80)]);
    let mut limits = Limits::default();
    limits.fits_header_cards = 8;
    let err = sequential(bytes, limits).expect_err("the card cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("more than 8 cards"), "{err}");
}

/// Criterion *Every cap has a test that trips it* — `hdus_per_advance`, and with it the
/// decline table's row *HDU-traversal cap tripped while advancing*.
///
/// The skip loop inside one `next_image()` is bounded individually by the header and
/// skipped-block caps but unbounded in *number*; over a pipe there is no file length to
/// terminate it, and a hang is what invariant I5 names alongside panics.
///
/// The row's three facts are graded here too — `LimitExceeded`, from `next_image()`, with no
/// `Header` — since they are the same trip over the same fixture.
#[test]
fn hdus_per_advance_cap_trips() {
    let bytes = file(&[
        empty_primary(),
        table_extension(),
        table_extension(),
        table_extension(),
        unsigned_2x2_extension(),
    ]);
    let mut limits = Limits::default();
    limits.hdus_per_advance = 2;
    let mut reader = sequential(bytes.clone(), limits).expect("the primary header parses");
    let err = reader.next_image().expect_err("the traversal cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("HDUs traversed"), "{err}");
    assert!(
        reader.current_header().is_err(),
        "no Header for a failed advance"
    );

    // Raised past the four positions the walk steps over, the same file reaches its image.
    let mut limits = Limits::default();
    limits.hdus_per_advance = 4;
    let mut reader = sequential(bytes, limits).expect("the primary header parses");
    assert!(reader.next_image().expect("the walk reaches the image"));
}

/// Criterion *Every cap has a test that trips it* — `total_samples`.
///
/// It counts the file's `width × height × channels`, before any buffer is sized and before any
/// sample width is known.
#[test]
fn total_samples_cap_trips() {
    let mut limits = Limits::default();
    limits.total_samples = 8;
    let mut reader = sequential(file(&[unsigned_4x4()]), limits).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 16];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("the sample cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("total samples"), "{err}");
}

/// Criterion *Every cap has a test that trips it* — `total_samples` **under
/// `select_channel`**.
///
/// The cap counts the *file's* geometry whether or not the reader was narrowed. Counting the
/// narrowed channel instead is the plausible implementation, and no other case distinguishes
/// the two: here the narrowed destination is 4 samples, well inside the cap the file's 12
/// exceed.
#[test]
fn total_samples_cap_counts_the_files_channels_under_select_channel() {
    let mut limits = Limits::default();
    limits.total_samples = 8;
    let mut reader = sequential(file(&[unsigned_2x2x3()]), limits).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    reader.select_channel(1).expect("the channel exists");
    assert_eq!(
        reader.current_header().ok().and_then(|h| h.channels()),
        Some(1),
        "the reader is narrowed"
    );
    let mut dst = [0.0f32; 4];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("the file's geometry still carries it past the cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("2x2x3"), "{err}");
}

/// Criterion *Every cap has a test that trips it* — the **pixel-phase order**.
///
/// The total-samples cap is evaluated first, on the file's declared geometry alone, before the
/// declared-size-versus-geometry cross-check and before the byte caps. The fixture would trip
/// all three: its geometry exceeds the sample cap, its stored bytes exceed the known length of
/// the source, and its destination exceeds the output-byte cap. Fixing the order is what makes
/// a frame declaring an impossible sample count `LimitExceeded` rather than `Malformed` on the
/// size cross-check.
#[test]
fn the_pixel_phase_evaluates_the_total_samples_cap_first() {
    let whole = file(&[unsigned_4x4()]);
    let header_only = whole[..2880].to_vec();
    let mut limits = Limits::default();
    limits.total_samples = 8;
    limits.decoded_output_bytes = 8;

    // Tier 3 has no destination at all, so this grades the cap against the cross-check alone.
    let mut reader = Reader::seekable_with_limits(Cursor::new(header_only.clone()), limits)
        .expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let err = reader
        .chunks()
        .next_chunk()
        .expect_err("the sample cap precedes the geometry-versus-length check");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("total samples"), "{err}");

    // Tier 2 adds a destination, and the byte cap that measures it must still run second.
    let mut reader =
        Reader::seekable_with_limits(Cursor::new(header_only), limits).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 16];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("the sample cap precedes the byte caps");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("total samples"), "{err}");
}

/// Criterion *Every cap has a test that trips it* — `decoded_output_bytes`.
#[test]
fn decoded_output_bytes_cap_trips() {
    let mut limits = Limits::default();
    limits.decoded_output_bytes = 8; // the 2x2 frame writes 16
    let mut reader = sequential(file(&[unsigned_2x2()]), limits).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 4];
    let err = reader.read_image_into(&mut dst).expect_err("the byte cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("decoded output bytes"), "{err}");
}

/// Criterion *Every cap has a test that trips it* — **`Limits` moves in both directions**,
/// since a parameter nothing moves is a constant. Run for `decoded_output_bytes`, the cap the
/// drizzle case moves.
///
/// The lowered direction is graded against `Limits::default()`. The raised direction is graded
/// against the lowered cap rather than against the 2³⁰ default: a fixture tripping that
/// default needs a 268-megapixel destination — a gibibyte of `f32` from a half-gigabyte file —
/// which is not a committed fixture. What is being graded is that the number in `Limits` is
/// what decides, in both directions, and that is what these three decodes show.
#[test]
fn limits_move_in_both_directions_for_decoded_output_bytes() {
    let bytes = file(&[unsigned_2x2()]);
    let want = unsigned_2x2_expected();

    // Clean under the defaults.
    let mut reader = sequential(bytes.clone(), Limits::default()).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 4];
    reader.read_image_into(&mut dst).expect("the frame decodes");
    assert_same_bits(&dst, &want, "under Limits::default()");

    // Refused under a lowered cap — the same bytes, one number changed.
    let mut lowered = Limits::default();
    lowered.decoded_output_bytes = 15; // one byte short of what the frame writes
    let mut reader = sequential(bytes.clone(), lowered).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 4];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("the lowered cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");

    // And decoding again once the cap is raised to exactly what the frame needs.
    let mut raised = lowered;
    raised.decoded_output_bytes = 16;
    let mut reader = sequential(bytes, raised).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 4];
    reader
        .read_image_into(&mut dst)
        .expect("the raised cap admits the frame");
    assert_same_bits(&dst, &want, "under the raised cap");
}

/// Criterion *Every cap has a test that trips it* — `materialized_bytes`.
///
/// It governs every buffer this crate allocates for itself, measured at that buffer's own size
/// before it is sized. On the FITS path that is the row-shaped staging buffer: one row of the
/// 4x4 fixture is 4 samples at 2 bytes.
#[test]
fn materialized_bytes_cap_trips() {
    let mut limits = Limits::default();
    limits.materialized_bytes = 4;
    let mut reader = sequential(file(&[unsigned_4x4()]), limits).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 16];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("one row is above the cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("materialized bytes"), "{err}");
}

/// Criterion *Every cap has a test that trips it* — `skipped_block_bytes`, and the half of its
/// rule that is easy to miss: it **bounds a read, not an allocation**, so it does not apply
/// where skipping is a seek.
///
/// The same bytes are walked twice. On a sequential source the extension's data unit must be
/// read and discarded to step over it, and the cap refuses it. On a seekable source the cursor
/// moves without transferring a byte, the cap is not consulted, and the walk reaches the image
/// beyond — which is what keeps an ordinary MEF whose `BINTABLE` heap exceeds the cap readable.
#[test]
fn skipped_block_bytes_bounds_a_read_and_not_a_seek() {
    let heap = Hdu::extension("BINTABLE")
        .card("BITPIX", "8")
        .card("NAXIS", "2")
        .card("NAXIS1", "2880")
        .card("NAXIS2", "2")
        .card("PCOUNT", "0")
        .card("GCOUNT", "1")
        .data(vec![0u8; 5760]);
    let bytes = file(&[empty_primary(), heap, unsigned_2x2_extension()]);
    let mut limits = Limits::default();
    limits.skipped_block_bytes = 2880; // the data unit is 5760 bytes

    let mut reader = sequential(bytes.clone(), limits).expect("the primary header parses");
    let err = reader
        .next_image()
        .expect_err("a sequential source must read in order to skip");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("skipped block bytes"), "{err}");

    let mut reader = Reader::seekable_with_limits(Cursor::new(bytes), limits)
        .expect("the primary header parses");
    assert!(
        reader.next_image().expect("the seek is not a read"),
        "the same bytes walk past cleanly through a seekable source"
    );
    let mut dst = [0.0f32; 4];
    reader
        .read_image_into(&mut dst)
        .expect("the image beyond the heap decodes");
    assert_same_bits(&dst, &unsigned_2x2_expected(), "past an oversized heap");
}

/// Criterion *Every cap has a test that trips it* — `images_per_source`.
///
/// It counts `next_image()` advances, and FITS MEF is where it binds.
#[test]
fn images_per_source_cap_trips() {
    let bytes = file(&[unsigned_2x2(), unsigned_2x2_extension()]);
    let mut limits = Limits::default();
    limits.images_per_source = 1;
    let mut reader = sequential(bytes.clone(), limits).expect("the header parses");
    assert!(
        reader
            .next_image()
            .expect("the first advance is inside the cap")
    );
    let err = reader.next_image().expect_err("the second advance is not");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("images per source"), "{err}");

    // Raised to exactly the number of images the source holds, the walk runs to its end. The
    // cap counts occurrences, so the terminating advance -- the one that reports end of source
    // and admits no image -- must not count against it; if it did, no source could ever be
    // walked to its end at a cap equal to its own image count.
    let mut limits = Limits::default();
    limits.images_per_source = 2;
    let mut reader = sequential(bytes, limits).expect("the header parses");
    assert!(reader.next_image().expect("the primary image"));
    assert!(reader.next_image().expect("the image extension"));
    assert!(
        !reader
            .next_image()
            .expect("the terminating advance is not an occurrence"),
        "the walk ends rather than tripping the cap it exactly meets"
    );
}

/// Criterion *Every cap has a test that trips it*, the geometry-versus-known-length check that
/// sits beside them: a known-length source whose geometry needs more stored bytes than the file
/// contains is `Malformed` **in the pixel phase** — and a `Reader::seekable` over a size-capped
/// *prefix* still parses its header and reports geometry with no error.
///
/// Refusing the prefix early would break header-only decode for a consumer fetching a
/// size-capped prefix of a remote file, which is the whole point of tier 1.
#[test]
fn geometry_beyond_a_known_length_is_malformed_only_in_the_pixel_phase() {
    let whole = file(&[unsigned_4x4()]);
    let header_only = whole[..2880].to_vec();
    let mut reader =
        Reader::seekable(Cursor::new(header_only)).expect("a header-only prefix parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let header = reader.current_header().expect("the header is reported");
    assert!(
        header.decline_reason().is_none(),
        "a prefix is not a declined position: {:?}",
        header.decline_reason()
    );
    assert_eq!(
        (header.width(), header.height(), header.channels()),
        (Some(4), Some(4), Some(1)),
        "geometry is reported from a source that cannot hold the pixels"
    );

    let mut dst = [0.0f32; 16];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("the pixels are not there");
    // Malformed rather than a cap: the file contradicts itself against its own length.
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(format!("{err}").contains("bytes long"), "{err}");
}

/// *Every cap has a test that trips it* — the clause that belongs to the `targets` lane's
/// `i686` job.
///
/// Every size computation runs in `u64` with checked arithmetic and is narrowed exactly once,
/// at the allocation site, after the relevant cap has been enforced. **Because a caller may
/// raise a cap past what a 32-bit target can address, that narrowing is checked rather than
/// infallible**: a `u64` that does not fit `usize` is `LimitExceeded`, the same class the cap
/// itself raises, and never a panic.
///
/// The shipped defaults keep this unreachable, which is what their sub-`i32::MAX` values buy.
/// The drizzle case makes it reachable on purpose — 549 megapixels of `f32` is 2.2 GB and fits
/// no 32-bit `usize` — so a caller who raises the cap for a legitimate frame must still get an
/// error rather than an abort.
///
/// Compiled only where `usize` is 32 bits; on a 64-bit host there is no width to overflow.
#[cfg(target_pointer_width = "32")]
#[test]
fn a_cap_raised_past_what_usize_addresses_is_limit_exceeded_not_a_panic() {
    use astroframe::{Error, Limits, Reader};
    use std::io::Cursor;

    // A geometry whose sample count exceeds u32::MAX, so the narrowing cannot succeed.
    let bytes = common::file(&[Hdu::primary()
        .image_2d(16, 100_000, 100_000)
        .unsigned_convention(16)]);

    // Raise every cap that would otherwise catch this first, so the narrowing is what the
    // frame actually reaches. Without this the test would pass on the total-samples cap and
    // assert nothing about the narrowing at all.
    let mut limits = Limits::default();
    limits.total_samples = u64::MAX;
    limits.decoded_output_bytes = u64::MAX;
    limits.materialized_bytes = u64::MAX;

    let mut reader = Reader::seekable_with_limits(Cursor::new(bytes), limits).expect("construct");
    assert!(reader.next_image().expect("advance"));

    // The destination length is computed before it is checked against the slice, so an empty
    // slice is enough to reach the narrowing.
    let err = reader
        .read_image_into(&mut [])
        .expect_err("must not succeed");
    assert!(
        matches!(err, Error::LimitExceeded(_)),
        "expected LimitExceeded at the narrowing, got {err:?}"
    );
}

/// Criterion *Every cap has a test that trips it* — `total_samples`, at the geometry where the
/// arithmetic itself is the hazard rather than the size.
///
/// Three `u32` axes reach 2^96, which no `u64` holds. `NAXIS1 = NAXIS2 = NAXIS3 = 4294967295`
/// is under three kilobytes of header; multiplying the three plainly panics wherever overflow
/// checks are on and wraps everywhere else, and the wrap is what defeats the cap — a product
/// congruent to something small modulo 2^64 is reported as a small sample count and walks
/// through the check that exists to reject the declaration. Both formats reach the same
/// arithmetic, so both are graded; the XISF half is `adversarial::geometry_whose_axis_product_exceeds_u64`.
#[test]
fn a_geometry_whose_axis_product_exceeds_u64_trips_the_total_samples_cap() {
    let max = u32::MAX.to_string();
    for (case, axes) in [
        ("saturating", [max.as_str(), max.as_str(), max.as_str()]),
        // 2^17 x 2^16 x 2^31 is exactly 2^64: wrapping arithmetic reports zero samples.
        ("wrapping to zero", ["131072", "65536", "2147483648"]),
    ] {
        let bytes = file(&[Hdu::primary()
            .card("BITPIX", "16")
            .card("NAXIS", "3")
            .card("NAXIS1", axes[0])
            .card("NAXIS2", axes[1])
            .card("NAXIS3", axes[2])
            .unsigned_convention(16)
            .data(vec![0u8; 8])]);

        let mut reader = sequential(bytes, Limits::default()).expect("the header parses");
        assert!(
            reader.next_image().expect("the position is reached"),
            "{case}: the geometry is representable, so the position is not skipped"
        );
        // Header-phase reporting says what the file declared; nothing has multiplied yet.
        let header = reader.current_header().expect("a header");
        assert!(
            header.decline_reason().is_none(),
            "{case}: a large geometry is not a decline"
        );

        let err = reader
            .read_image()
            .expect_err("{case}: the pixel phase refuses it");
        assert_eq!(kind(&err), "LimitExceeded", "{case}: {err}");
        assert!(format!("{err}").contains("total samples"), "{case}: {err}");
    }
}

/// The FITS half of `adversarial::a_geometry_whose_byte_extent_exceeds_u64_under_a_raised_cap`.
///
/// Same shape, same reachability condition: the total-samples cap raised to `u64::MAX`, which
/// § The caps contemplates a caller doing, after which the saturating sample count flows into
/// products that used to be plain `*` and panicked under the overflow checks every test build
/// carries. Both formats share the gate, so both are graded against it.
#[test]
fn a_geometry_whose_byte_extent_exceeds_u64_under_a_raised_cap() {
    let max = u32::MAX.to_string();
    let bytes = file(&[Hdu::primary()
        .card("BITPIX", "16")
        .card("NAXIS", "2")
        .card("NAXIS1", &max)
        .card("NAXIS2", &max)
        .unsigned_convention(16)
        .data(vec![0u8; 8])]);

    let mut raised = Limits::default();
    raised.total_samples = u64::MAX;
    raised.decoded_output_bytes = u64::MAX;
    raised.materialized_bytes = u64::MAX;

    let mut reader = sequential(bytes, raised).expect("the header parses");
    assert!(reader.next_image().expect("the position is reached"));
    let err = reader
        .read_image()
        .expect_err("a byte extent above u64 is refused rather than computed");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("total samples"), "{err}");
}

/// **Why the FITS row offset needs no check of its own, asserted rather than argued.**
///
/// `next_chunk` computes `data_start + file_row * row_bytes`, which is the same shape as the
/// XISF site that *did* overflow (`adversarial::a_row_offset_past_u64_is_refused_rather_than_
/// computed`). It is safe here for a reason worth pinning: `file_row * row_bytes` is strictly
/// less than the frame's byte extent, and the byte-extent gate refuses any geometry whose
/// extent does not fit `u64`. So a passing gate leaves the whole `u64` range above the extent
/// free, and `data_start` — which is a position the source actually reached, not a number the
/// file declared — cannot exceed the bytes produced.
///
/// That last clause is the difference between the two formats. XISF's `position` comes out of
/// a `location` attribute and a file states it; FITS's `data_start` is observed. A declared
/// offset needs its own arithmetic checked, an observed one is bounded by the reading.
///
/// The three-axis spelling with `select_channel` is the shape that puts `file_row` at
/// `k x height` on the first chunk rather than after iterating, which is the one that would
/// reach it first if the reasoning above were wrong.
#[test]
fn a_row_offset_past_u64_is_refused_rather_than_computed() {
    let max = u32::MAX.to_string();
    let bytes = file(&[Hdu::primary()
        .card("BITPIX", "64")
        .card("NAXIS", "3")
        .card("NAXIS1", &max)
        .card("NAXIS2", &max)
        .card("NAXIS3", "2")
        .data(vec![0u8; 8])]);

    let mut raised = Limits::default();
    raised.total_samples = u64::MAX;
    raised.decoded_output_bytes = u64::MAX;
    raised.materialized_bytes = u64::MAX;

    let mut reader = sequential(bytes, raised).expect("the header parses");
    assert!(reader.next_image().expect("the position is reached"));
    // The last channel, so the first row of the decode already sits at `k x height`.
    reader.select_channel(1).expect("the channel exists");
    let err = reader
        .chunks()
        .next_chunk()
        .expect_err("a row offset above u64 is refused rather than computed");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("total samples"), "{err}");
}
