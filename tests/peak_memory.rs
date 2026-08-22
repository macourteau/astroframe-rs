//! *Peak decode memory meets the stated target* — the allocation half.
//!
//! The criterion has **two forms, because a scheduled-only check lets the regression it
//! exists to catch merge green**: an allocation bound on every push (this file), and a
//! resident-memory measurement on the scheduled lane, where a quiet machine makes it
//! meaningful (`tests/peak_memory_resident.rs`).
//!
//! Both express the same thing as a multiple of the **destination buffer**. The destination
//! is allocated before the measured region begins, so what is counted here is everything the
//! decode allocates *on top of* it — which is the interesting quantity and the one that
//! separates a streaming decode from one that materializes the file.
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
use common::xisf::{Unit, lz4, repeating_u16};
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
    assert_eq!(
        reader.header().expect("a header").granularity(),
        astroframe::Granularity::Block {
            subblocks: PARTS as u32
        },
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
