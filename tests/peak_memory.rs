//! *Peak decode memory meets the stated target* — the allocation half.
//!
//! The criterion has **two forms, because a check no lane runs lets the regression it exists
//! to catch merge green**: an allocation bound on every push (this file), and a
//! resident-memory measurement invoked by hand, where a quiet machine makes it meaningful
//! (`tests/peak_memory_resident.rs`).
//!
//! Both express the same thing as a multiple of the **destination buffer**. The destination
//! is allocated before the measured region begins, so what is counted here is everything the
//! decode allocates *on top of* it — which is the interesting quantity and the one that
//! separates a streaming decode from one that materializes the file.
//!
//! **The peak is not the only quantity, and the last shape in the file measures the other
//! one.** § Fuzzing's oracle counts every allocation rather than the high-water mark, so a
//! buffer released at each subblock boundary and built again at the next is free to the peak
//! and charged in full to the oracle. That shape is bounded against the **input** rather than
//! against the destination, since what it asserts is `docs/intentional-patterns.md`'s rule
//! about allocations sized by a cap.
//!
//! Without this, an implementation that buffers the whole file passes every other criterion
//! while missing the entire point of tier 2.
//!
//! **Both measurements live in one `#[test]`, deliberately.** A `#[global_allocator]` counts
//! the whole process, and the test harness runs `#[test]`s on parallel threads — so a second
//! test building its fixture lands in the counter this one just reset. Measured that way the
//! compressed case reported 60 MB against a model of 9 MB, which looks exactly like a decoder
//! defect and is not one. One test, run sequentially, is what makes the number mean
//! something.

#[path = "common/alloc.rs"]
mod alloc;
mod common;

use astroframe::Reader;
use common::Hdu;
use common::xisf::{Unit, lz4, repeating_u16, shuffle, zlib};
use std::io::Cursor;

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// A 25 MP mono frame — large enough that the ratios below mean something.
const WIDTH: u32 = 6144;
const HEIGHT: u32 = 4096;

/// Write a fixture to a scratch file and hand back its path.
///
/// A real file rather than an in-memory `Cursor`: a 52 MB fixture held in memory would be
/// counted as decode cost and the measurement would say nothing.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str, bytes: &[u8]) -> Scratch {
        let path =
            std::env::temp_dir().join(format!("astroframe-{name}-{}.bin", std::process::id()));
        std::fs::write(&path, bytes).expect("write the scratch fixture");
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Both rows of the peak-memory table, measured one after the other in a single test.
///
/// The unstreamed path costs about **2.05×** the output for an integer frame, holding
/// the raw bytes, a full typed intermediate and the `f32` output at once — 2 + 2 + 4 bytes per
/// pixel. Row-streaming removes both leading terms, which is the measured halving this
/// asserts.
///
/// The threshold is loose enough to absorb allocator behaviour and a decode buffer, and tight
/// enough that **materializing the file would fail it**: the fixture is 52 MB against a 26 MB
/// allowance.
#[test]
fn peak_decode_memory_meets_the_stated_target() {
    fits_row();
    compressed_xisf_row();
    subblocked_lz4_row();
    // Both framed codecs, because each carries a codec state of its own across the split.
    a_subblock_costs_the_subblock_and_not_the_cap("zlib", &zlib);
    a_subblock_costs_the_subblock_and_not_the_cap("zstd", &zstd_raw);
    // And both again on the materializing path, which is a second walk over the same split.
    a_materialized_subblock_costs_the_subblock_and_not_the_cap("zlib", &zlib);
    a_materialized_subblock_costs_the_subblock_and_not_the_cap("zstd", &zstd_raw);
}

fn fits_row() {
    let samples = WIDTH as usize * HEIGHT as usize;
    let stored: Vec<i16> = (0..samples)
        .map(|i| ((i % 65536) as i32 - 32768) as i16)
        .collect();
    let bytes = common::file(&[Hdu::primary()
        .image_2d(16, WIDTH, HEIGHT)
        .unsigned_convention(16)
        .data_i16(&stored)]);
    assert!(
        bytes.len() > 50_000_000,
        "the fixture must be big enough for the bound to bite"
    );
    let scratch = Scratch::new("peak-fits", &bytes);
    drop(bytes);

    let mut dst = vec![0.0f32; samples];
    let destination_bytes = dst.len() * 4;

    let mut reader = Reader::open(&scratch.0).expect("open the fixture");
    assert!(reader.next_image().expect("advance"));

    alloc::reset();
    reader.read_image_into(&mut dst).expect("decode");
    let used = alloc::total();

    // 1.25x the destination, minus the destination itself.
    let allowed = destination_bytes / 4;
    assert!(
        used <= allowed,
        "FITS decode allocated {used} bytes above a {destination_bytes}-byte destination; the \
         1.25x target allows {allowed}. A decode that materialized the {} byte file would land \
         here.",
        std::fs::metadata(&scratch.0).map(|m| m.len()).unwrap_or(0)
    );
    assert_eq!(dst[0].to_bits(), 0x0000_0000);
}

/// The compressed row of the peak-memory table.
///
/// Deliberately worse than the FITS figure, because that is what this codec costs: a bare LZ4
/// block must be fully resident before it decompresses, so the peak is stored block +
/// decompressed block + destination. The model gives 2.0× at 2 + 2 + 4 bytes per pixel and
/// the threshold carries the rest as margin for the allocator and for the compressed block's
/// excess over its uncompressed size, which is bounded only by the stored-block cap. **A
/// threshold no implementation can fail would defeat the criterion's purpose.**
fn compressed_xisf_row() {
    let width = 2048u32;
    let height = 2048u32;
    let samples = width as usize * height as usize;
    let data = repeating_u16(samples);
    let raw = common::xisf::le_u16(&data);
    let compressed = lz4(&raw);

    let template = format!(
        r#"<Image geometry="{width}:{height}:1" sampleFormat="UInt16" compression="lz4:{}" {{loc}}/>"#,
        raw.len()
    );
    let unit = Unit::new().attached(&template, compressed).build();

    let mut dst = vec![0.0f32; samples];
    let destination_bytes = dst.len() * 4;

    let mut reader = Reader::seekable(Cursor::new(&unit)).expect("open");
    assert!(reader.next_image().expect("advance"));

    alloc::reset();
    reader.read_image_into(&mut dst).expect("decode");
    let used = alloc::total();

    // 2.6x the destination, minus the destination itself.
    let allowed = destination_bytes * 8 / 5;
    assert!(
        used <= allowed,
        "compressed XISF decode allocated {used} bytes above a {destination_bytes}-byte \
         destination; the 2.6x target allows {allowed}"
    );
}

/// The `lz4` **+ `subblocks`** row, which is the only way `Block` granularity is reachable at
/// all: LZ4 has no framing, so the split is what makes anything smaller than the whole block
/// decodable, and § Streaming puts this row's peak at *one subblock, compressed and
/// decompressed* rather than the whole image.
///
/// Reporting `Block` while materializing the whole block would pass every other criterion —
/// the pixels come out right either way — so the reported floor is only worth something if
/// something measures it. The threshold is set below what the monolithic path costs on the
/// same fixture, which is what makes it discriminate rather than merely hold.
fn subblocked_lz4_row() {
    const PARTS: usize = 8;
    let width = 2048u32;
    let height = 2048u32;
    let samples = width as usize * height as usize;
    let raw = common::xisf::le_u16(&repeating_u16(samples));

    // Split over the *compression*, per §10.6: each piece is compressed on its own.
    let part = raw.len() / PARTS;
    let mut stored = Vec::new();
    let mut list = Vec::new();
    for chunk in raw.chunks(part) {
        let c = lz4(chunk);
        list.push(format!("{},{}", c.len(), chunk.len()));
        stored.extend_from_slice(&c);
    }
    let list = list.join(":");

    let template = format!(
        r#"<Image geometry="{width}:{height}:1" sampleFormat="UInt16" compression="lz4:{}" subblocks="{list}" {{loc}}/>"#,
        raw.len()
    );
    let unit = Unit::new().attached(&template, stored).build();

    let mut dst = vec![0.0f32; samples];
    let destination_bytes = dst.len() * 4;

    let mut reader = Reader::seekable(Cursor::new(&unit)).expect("open");
    assert!(reader.next_image().expect("advance"));
    assert!(
        matches!(
            reader.header().expect("a header").granularity(),
            astroframe::Granularity::Block { subblocks, .. } if subblocks == PARTS as u32
        ),
        "the fixture must actually reach the Block floor for the bound below to mean anything"
    );

    alloc::reset();
    reader.read_image_into(&mut dst).expect("decode");
    // The **peak**, not the cumulative total: the two are identical for a single-pass decode
    // but not for this one, whose whole point is that the same buffer serves each subblock in
    // turn. Measured cumulatively, holding one subblock and holding all eight cost the same.
    let used = alloc::peak();

    // One decompressed subblock is destination_bytes / 16: `raw` is `u16` at half the
    // destination's `f32` width, and one subblock is one of `PARTS`. The threshold allows
    // 1.5x that, absorbing the stored (compressed) buffer and allocator overhead, while
    // staying below what holding the outgoing and incoming subblocks' buffers together costs
    // — the finding-7 regression this bound exists to catch, measured at destination_bytes /
    // ~7.95 on this fixture.
    let allowed = destination_bytes * 3 / 32;
    assert!(
        used <= allowed,
        "subblocked LZ4 decode allocated {used} bytes above a {destination_bytes}-byte \
         destination; holding one of {PARTS} subblocks at a time allows {allowed}. A decode \
         that holds the outgoing subblock's buffers alongside the incoming one's lands here."
    );
}

/// A zstd frame built from **raw** (stored) blocks, with a four-byte content size.
///
/// `zstd` appears nowhere in XISF 1.0 and this crate's support for it is corpus-derived, so
/// the fixture is a frame written here byte by byte rather than one produced by an encoder the
/// crate does not depend on. `Single_Segment_flag` makes the declared window the content size,
/// which keeps every split below the `zstd_window_bytes` cap.
fn zstd_raw(input: &[u8]) -> Vec<u8> {
    assert!(input.len() < 128 * 1024, "one Raw_Block's maximum size");
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd];
    out.push(0xa0); // Single_Segment_flag, and a four-byte Frame_Content_Size
    out.extend_from_slice(&(input.len() as u32).to_le_bytes());
    let block_header: u32 = ((input.len() as u32) << 3) | 1; // last block, Raw_Block
    out.extend_from_slice(&block_header.to_le_bytes()[..3]);
    out.extend_from_slice(input);
    out
}

/// The **cumulative** half of the same criterion, on the subblock axis: what a decode
/// allocates over a block must be sized by that block's stored bytes, never by
/// `Subblock count`.
///
/// The two measures come apart exactly here, which is why this shape exists beside
/// `subblocked_lz4_row`. §10.6 restarts the codec at every subblock boundary, so anything a
/// boundary builds runs `subblocks` times — and a decode that releases each piece before
/// building the next has a flat *peak* however many pieces there are. § Fuzzing's oracle counts
/// every allocation rather than the peak, and `docs/intentional-patterns.md` states the rule
/// that count enforces: **no per-occurrence allocation may be sized by a cap**. A per-subblock
/// input window of a fixed 256 KiB satisfies the peak bound above and violates that one, at
/// 4096 × 256 KiB from a hundred kilobytes of input.
///
/// So the assertion is made twice over, and the second is the discriminating one:
///
/// * against the fuzz oracle's own bound, `32 × input + 8 MiB`, which is what a fuzz run
///   would report; and
/// * against the **same block split more ways** — the subblock count rises by 512× while the
///   pixels and the geometry stay as they were, so an honest decode's total barely moves. A
///   shape at one subblock count cannot say this: allocation linear in the split passes any
///   single-point bound loose enough to hold at all.
///
/// Run for both framed codecs. Each carries a codec state across the split — a
/// `flate2::Decompress` is about 43 KB to construct and a `ruzstd::FrameDecoder` about 9.5 KB
/// — so "one state per subblock" is a separate way to reach the same defect on each, and one
/// codec's shape does not hold the other's. LZ4 is not here: its buffers are sized from the
/// subblock already, which is what `subblocked_lz4_row` measures.
fn a_subblock_costs_the_subblock_and_not_the_cap(codec: &str, compress: &dyn Fn(&[u8]) -> Vec<u8>) {
    // 4096 is the `Subblock count` cap, so the wide point is the worst a file may ask for.
    const SPLITS: [usize; 2] = [8, 4096];
    const PER_SUBBLOCK: usize = 64;

    let samples = SPLITS[1] * PER_SUBBLOCK / 2;
    let width = PER_SUBBLOCK as u32 / 2;
    let height = SPLITS[1] as u32;
    let raw = common::xisf::le_u16(&repeating_u16(samples));

    let mut totals = Vec::new();
    for parts in SPLITS {
        // Split over the *compression*, per §10.6: each piece is compressed on its own.
        let part = raw.len() / parts;
        let mut stored = Vec::new();
        let mut list = Vec::new();
        for chunk in raw.chunks(part) {
            let c = compress(chunk);
            list.push(format!("{},{}", c.len(), chunk.len()));
            stored.extend_from_slice(&c);
        }
        let list = list.join(":");

        let template = format!(
            r#"<Image geometry="{width}:{height}:1" sampleFormat="UInt16" compression="{codec}:{}" subblocks="{list}" {{loc}}/>"#,
            raw.len()
        );
        let unit = Unit::new().attached(&template, stored).build();
        let input_bytes = unit.len();

        let mut dst = vec![0.0f32; samples];
        let mut reader = Reader::seekable(Cursor::new(&unit)).expect("open");
        assert!(reader.next_image().expect("advance"));
        assert_eq!(
            reader.header().expect("a header").granularity(),
            astroframe::Granularity::Rows,
            "§ Streaming puts a framed codec plus subblocks at `Rows`; a fixture reporting \
             anything else is not on the streaming subblock path this measures"
        );

        alloc::reset();
        reader.read_image_into(&mut dst).expect("decode");
        // The cumulative total, not the peak: a buffer released at each boundary and built
        // again at the next is invisible to `peak()` and is the whole subject here.
        let used = alloc::total();

        // The fuzz oracle's own bound, restated so the two cannot drift — `32 × input_length
        // + xml_header_bytes`, at § Fuzzing's `ALLOC_MULTIPLE` and the shipped 8 MiB cap.
        let allowed = 32 * input_bytes + (8 << 20);
        assert!(
            used <= allowed,
            "a {codec} block split {parts} ways allocated {used} bytes from a \
             {input_bytes}-byte input, above the fuzz oracle's {allowed}. A per-subblock \
             input window or codec state sized by `Subblock count` rather than by the \
             subblock lands here."
        );
        totals.push(used);
    }

    let (narrow, wide) = (totals[0], totals[1]);
    // Twice the narrow split's total: far under the 512× the split itself rises by, and far
    // over the variation between one codec state and the same one reset. A per-subblock cost
    // sized by anything but the subblock cannot fit under it.
    let allowed = narrow * 2;
    assert!(
        wide <= allowed,
        "splitting the same {codec} block from {} into {} subblocks took allocation from \
         {narrow} to {wide} bytes, above the {allowed} a flat per-subblock cost allows. \
         Allocation that grows with the split is sized by the cap rather than by the \
         subblock.",
        SPLITS[0],
        SPLITS[1]
    );
}

/// The same cumulative bound on the **materializing** path, which walks the subblock list a
/// second time and reaches it from an ordinary file rather than an exotic one.
///
/// `a_subblock_costs_the_subblock_and_not_the_cap` measures the streaming decoder, and a
/// subblocked block is not always streamed: § Streaming's floors compose to `WholeImage` when
/// the split is joined by byte shuffling, because the shuffle spans the whole pre-split block,
/// and equally when a checksum covers the stored block the split does not divide. Both are
/// combinations a writer picks for reasons of its own, so the block-at-a-time decoder walks the
/// same `Subblock count` boundaries — and `docs/intentional-patterns.md`'s rule is about the
/// walk, not about which decoder performs it.
///
/// The shape is the streaming one's, and it asserts the same two things: the fuzz oracle's
/// `32 × input + 8 MiB`, and the same block split 512 times more ways. What differs is the
/// fixture's `+sh`, which is what puts the decode on this path, and the granularity assertion
/// that keeps it there — a fixture that drifted back to `Rows` would pass by measuring the
/// decoder this shape is not for.
fn a_materialized_subblock_costs_the_subblock_and_not_the_cap(
    codec: &str,
    compress: &dyn Fn(&[u8]) -> Vec<u8>,
) {
    // 4096 is the `Subblock count` cap, so the wide point is the worst a file may ask for.
    const SPLITS: [usize; 2] = [8, 4096];
    const PER_SUBBLOCK: usize = 64;

    let samples = SPLITS[1] * PER_SUBBLOCK / 2;
    let width = PER_SUBBLOCK as u32 / 2;
    let height = SPLITS[1] as u32;
    let levels = repeating_u16(samples);
    let raw = common::xisf::le_u16(&levels);
    // §10.6.2 shuffles the block as a whole, *before* the split, so the fixture shuffles once
    // and then divides the result — which is exactly why the split buys the decoder nothing
    // and the granularity floor is `WholeImage`.
    let shuffled = shuffle(&raw, 2);

    let mut totals = Vec::new();
    for parts in SPLITS {
        // Split over the *compression*, per §10.6: each piece is compressed on its own.
        let part = shuffled.len() / parts;
        let mut stored = Vec::new();
        let mut list = Vec::new();
        for chunk in shuffled.chunks(part) {
            let c = compress(chunk);
            list.push(format!("{},{}", c.len(), chunk.len()));
            stored.extend_from_slice(&c);
        }
        let list = list.join(":");

        let template = format!(
            r#"<Image geometry="{width}:{height}:1" sampleFormat="UInt16" compression="{codec}+sh:{}:2" subblocks="{list}" {{loc}}/>"#,
            raw.len()
        );
        let unit = Unit::new().attached(&template, stored).build();
        let input_bytes = unit.len();

        let mut dst = vec![0.0f32; samples];
        let mut reader = Reader::seekable(Cursor::new(&unit)).expect("open");
        assert!(reader.next_image().expect("advance"));
        assert_eq!(
            reader.header().expect("a header").granularity(),
            astroframe::Granularity::WholeImage,
            "a shuffled subblocked block is materialized; a fixture reporting anything else \
             is on the streaming path the shape above already measures"
        );

        alloc::reset();
        reader.read_image_into(&mut dst).expect("decode");
        // The cumulative total, not the peak: a codec state released at each boundary and
        // built again at the next is invisible to `peak()` and is the whole subject here.
        let used = alloc::total();

        // The pixels, because a bound met by a decode that unshuffles the wrong bytes says
        // nothing. `to_bits`, per the repository's rule about sign-of-zero.
        let expect = common::xisf::expected_u16(&levels);
        assert!(
            dst.iter()
                .zip(&expect)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "the {codec} fixture split {parts} ways did not round-trip through the shuffle"
        );

        // The fuzz oracle's own bound, restated so the two cannot drift — `32 × input_length
        // + xml_header_bytes`, at § Fuzzing's `ALLOC_MULTIPLE` and the shipped 8 MiB cap.
        let allowed = 32 * input_bytes + (8 << 20);
        assert!(
            used <= allowed,
            "a materialized {codec} block split {parts} ways allocated {used} bytes from a \
             {input_bytes}-byte input, above the fuzz oracle's {allowed}. A codec state built \
             per subblock rather than carried across the block lands here."
        );
        totals.push(used);
    }

    let (narrow, wide) = (totals[0], totals[1]);
    // Twice the narrow split's total: far under the 512× the split itself rises by, and far
    // over the variation between one codec state and the same one reset. A per-subblock cost
    // sized by anything but the subblock cannot fit under it.
    let allowed = narrow * 2;
    assert!(
        wide <= allowed,
        "splitting the same materialized {codec} block from {} into {} subblocks took \
         allocation from {narrow} to {wide} bytes, above the {allowed} a flat per-subblock \
         cost allows. Allocation that grows with the split is sized by the cap rather than by \
         the subblock.",
        SPLITS[0],
        SPLITS[1]
    );
}
