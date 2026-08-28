//! The layering criteria: one normalization serving both containers, delivery that does not
//! change bits, and the tiers behaving as documented.
//!
//! These are the tests invariants I1, I2 and I3 rest on, so every pixel comparison here is by
//! `f32::to_bits()` — `==` silently accepts a sign-of-zero difference, which is exactly the
//! class of defect an endpoint test exists to catch.
//!
//! Fixtures are built byte by byte through `tests/common`. Everything is synthetic: no real
//! frame header, no observatory coordinate at real precision, no absolute path.

#![forbid(unsafe_code)]

mod common;

use std::io::Cursor;
use std::ops::ControlFlow;

use astroframe::{
    Bounds, Format, Header, IterF64, Orientation, PixelStorage, Reader, RowOrder, Samples, Source,
};

use common::xisf::{self, Unit};
use common::{Hdu, Streams, assert_granularity, assert_same_bits, file, kind};

// ------------------------------------------------------------------ fixtures

/// The sample values every cross-format fixture carries.
///
/// `repeating_u16`'s cycle includes 257, 261, 265 and 269 — levels where the multiply and the
/// divide normalization forms differ. A fixture carrying none of them passes against a
/// divide-form implementation, so it grades nothing.
fn levels(n: usize) -> Vec<u16> {
    let v = xisf::repeating_u16(n);
    assert!(
        v.contains(&257),
        "the fixture must carry a divergent level or it grades nothing"
    );
    v
}

/// A FITS frame under the unsigned convention: `BITPIX = 16`, `BZERO = 32768`, `BSCALE = 1`.
///
/// Stored samples are the levels less 32768, so the *physical* values `BSCALE`/`BZERO` produce
/// are the levels themselves — which is what makes the XISF `UInt16` fixture beside it carry
/// the same sample values rather than merely the same bytes.
fn fits_unsigned_u16(width: u32, height: u32, channels: u32, levels: &[u16]) -> Vec<u8> {
    let stored: Vec<i16> = levels
        .iter()
        .map(|l| (i32::from(*l) - 32768) as i16)
        .collect();
    let hdu = Hdu::primary();
    let hdu = if channels == 1 {
        hdu.image_2d(16, width, height)
    } else {
        hdu.image_3d(16, width, height, channels)
    };
    file(&[hdu.unsigned_convention(16).data_i16(&stored)])
}

/// An uncompressed XISF `UInt16` frame over the same levels.
fn xisf_uncompressed(width: u32, height: u32, channels: u32, levels: &[u16]) -> Vec<u8> {
    Unit::new()
        .image_u16(width, height, channels, levels)
        .build()
}

/// A channel-planar XISF `UInt16` frame carrying an explicit `pixelStorage`.
fn xisf_planar(width: u32, height: u32, channels: u32, planar_levels: &[u16]) -> Vec<u8> {
    let template = format!(
        r#"<Image geometry="{width}:{height}:{channels}" sampleFormat="UInt16" colorSpace="RGB" pixelStorage="Planar" {{loc}}/>"#
    );
    Unit::new()
        .attached(&template, xisf::le_u16(planar_levels))
        .build()
}

/// The **same image** as [`xisf_planar`], written interleaved.
///
/// Taking the planar levels and transposing them here is what makes the two fixtures
/// comparable: the decoder's own transposition is then graded against one written
/// independently, rather than against itself.
fn xisf_interleaved(width: u32, height: u32, channels: u32, planar_levels: &[u16]) -> Vec<u8> {
    let plane = (width * height) as usize;
    let channels = channels as usize;
    let mut interleaved = vec![0u16; planar_levels.len()];
    for c in 0..channels {
        for i in 0..plane {
            interleaved[i * channels + c] = planar_levels[c * plane + i];
        }
    }
    let template = format!(
        r#"<Image geometry="{width}:{height}:{channels}" sampleFormat="UInt16" colorSpace="RGB" pixelStorage="Normal" {{loc}}/>"#
    );
    Unit::new()
        .attached(&template, xisf::le_u16(&interleaved))
        .build()
}

/// A shuffled, compressed XISF `UInt16` frame — `zlib+sh` or `lz4+sh`.
fn xisf_shuffled(codec: &str, width: u32, height: u32, channels: u32, levels: &[u16]) -> Vec<u8> {
    let raw = xisf::le_u16(levels);
    let shuffled = xisf::shuffle(&raw, 2);
    let stored = match codec {
        "zlib" => xisf::zlib(&shuffled),
        "lz4" => xisf::lz4(&shuffled),
        other => panic!("this suite builds no {other} fixture"),
    };
    let template = format!(
        r#"<Image geometry="{width}:{height}:{channels}" sampleFormat="UInt16" compression="{codec}+sh:{}:2" {{loc}}/>"#,
        raw.len()
    );
    Unit::new().attached(&template, stored).build()
}

/// An `lz4` XISF frame split into two subblocks and neither shuffled nor checksummed — the
/// only combination that reaches `Block` granularity, since `zlib` and `zstd` already stream
/// by rows and either a shuffle or a checksum forces `WholeImage`.
fn xisf_lz4_subblocks(width: u32, height: u32, channels: u32, levels: &[u16]) -> Vec<u8> {
    let raw = xisf::le_u16(levels);
    let split = raw.len() / 2;
    let first = xisf::lz4(&raw[..split]);
    let second = xisf::lz4(&raw[split..]);
    let mut stored = first.clone();
    stored.extend_from_slice(&second);
    let subblocks = format!(
        "{},{}:{},{}",
        first.len(),
        split,
        second.len(),
        raw.len() - split
    );
    let template = format!(
        r#"<Image geometry="{width}:{height}:{channels}" sampleFormat="UInt16" compression="lz4:{}" subblocks="{subblocks}" {{loc}}/>"#,
        raw.len()
    );
    Unit::new().attached(&template, stored).build()
}

// ------------------------------------------------------------------ decode helpers

/// Advance to the first image and hand back its header.
fn first_image<S: Source>(reader: &mut Reader<S>) -> Header {
    assert!(reader.next_image().expect("advance"), "a first image");
    reader.current_header().expect("the advanced position")
}

/// Whole-buffer decode of the currently selected image.
fn whole_buffer<S: Source>(reader: &mut Reader<S>, len: usize) -> Vec<f32> {
    let mut dst = vec![0.0f32; len];
    reader
        .read_image_into(&mut dst)
        .expect("whole-image decode");
    dst
}

/// The same image assembled from `chunks()`, copying each chunk at `chunk.range()`.
///
/// Tier 3 delivers *native* samples, so assembling a normalized buffer means running the
/// **shipped** primitive: `Reader::normalizer` for the range actually in force, then
/// `Chunk::normalize_into` per chunk. Reimplementing either here would grade a copy of the
/// crate's arithmetic against the crate's, which is the tautology the bit-identity criterion
/// exists to avoid.
///
/// Copying at the stated range is what makes the destination-coordinates contract meaningful:
/// a chunk consumer recalculating the offset itself would grade its own arithmetic instead.
fn from_chunks<S: Source>(reader: &mut Reader<S>, len: usize) -> Vec<f32> {
    let n = reader.normalizer().expect("the fixture has a range");
    let mut dst = vec![f32::NAN; len];
    let mut cursor = reader.chunks();
    while let Some(chunk) = cursor.next_chunk().expect("a chunk") {
        let range = chunk.range();
        chunk.normalize_into(&n, &mut dst[range]);
    }
    dst
}

/// The same image assembled through the **push** form.
///
/// `for_each_chunk` is a wrapper over the pull form, so this grades the wrapper — that it
/// delivers every chunk exactly once and hands the callback the same ranges — rather than the
/// delivery machinery a second time.
fn from_for_each_chunk<S: Source>(reader: &mut Reader<S>, len: usize) -> Vec<f32> {
    let n = reader.normalizer().expect("the fixture has a range");
    let mut dst = vec![f32::NAN; len];
    reader
        .for_each_chunk(|chunk| {
            let range = chunk.range();
            chunk.normalize_into(&n, &mut dst[range]);
            ControlFlow::Continue(())
        })
        .expect("push-form delivery");
    dst
}

/// The whole-buffer decode of a fixture's first image, through a seekable in-memory source.
fn decode_first(bytes: &[u8]) -> Vec<f32> {
    let mut reader = Reader::seekable(Cursor::new(bytes.to_vec())).expect("construct");
    let header = first_image(&mut reader);
    let len = expected_len(&header);
    whole_buffer(&mut reader, len)
}

fn expected_len(header: &Header) -> usize {
    let g = header.geometry().expect("geometry");
    g.width as usize * g.height as usize * g.channels as usize
}

/// The `f32` a level normalizes to under the default 16-bit range, written out longhand.
fn want_u16(level: u16) -> f32 {
    level as f32 * (1.0f32 / 65535.0f32)
}

/// A scratch path under the platform's temp directory. Never a hardcoded one: a machine-local
/// absolute path in committed source is what the CI grep fails on.
fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("astroframe-pipeline-{}-{name}", std::process::id()))
}

// ================================================================== I1: cross-format

/// One half of criterion *Cross-format bit-identity*, for one XISF spelling of the frame.
fn cross_format_identity(xisf_bytes: &[u8], what: &str) {
    let data = levels(32);
    let fits = decode_first(&fits_unsigned_u16(8, 4, 1, &data));
    let xisf = decode_first(xisf_bytes);

    // The reference form longhand, so a defect shared by both decoders cannot pass.
    let want: Vec<f32> = data.iter().map(|l| want_u16(*l)).collect();
    assert_same_bits(&fits, &want, "FITS against the pinned form");
    assert_same_bits(
        &xisf,
        &want,
        &format!("XISF {what} against the pinned form"),
    );
    assert_same_bits(&xisf, &fits, &format!("XISF {what} against FITS"));
}

/// Criterion *Cross-format bit-identity* (invariant I1), uncompressed.
#[test]
fn cross_format_bit_identity_uncompressed() {
    let data = levels(32);
    cross_format_identity(&xisf_uncompressed(8, 4, 1, &data), "uncompressed");
}

/// Criterion *Cross-format bit-identity*, `zlib+sh` — compression and shuffling proven not to
/// perturb the output.
#[test]
fn cross_format_bit_identity_zlib_shuffled() {
    let data = levels(32);
    cross_format_identity(&xisf_shuffled("zlib", 8, 4, 1, &data), "zlib+sh");
}

/// Criterion *Cross-format bit-identity*, `lz4+sh`.
#[test]
fn cross_format_bit_identity_lz4_shuffled() {
    let data = levels(32);
    cross_format_identity(&xisf_shuffled("lz4", 8, 4, 1, &data), "lz4+sh");
}

// ================================================================== I2: delivery

/// Criterion *Streaming equals whole-buffer, bit-for-bit* (invariant I2).
///
/// Run for both formats and for each streaming granularity, since granularity is the axis
/// along which the delivery machinery actually differs.
#[test]
fn streaming_equals_whole_buffer_bit_for_bit() {
    let data = levels(32);
    let cube = levels(36);
    let cases: [(&str, Vec<u8>, Streams, &[u16]); 6] = [
        (
            "FITS",
            fits_unsigned_u16(8, 4, 1, &data),
            Streams::Rows,
            &data,
        ),
        (
            "FITS NAXIS = 3",
            fits_unsigned_u16(4, 3, 3, &cube),
            Streams::Rows,
            &cube,
        ),
        (
            "XISF uncompressed",
            xisf_uncompressed(8, 4, 1, &data),
            Streams::Rows,
            &data,
        ),
        (
            // The interleaved path transposes a row at a time, so its chunk ranges are the
            // ones a destination-coordinate error shows up in first.
            "XISF Normal, three channels",
            xisf_interleaved(4, 3, 3, &cube),
            Streams::Rows,
            &cube,
        ),
        (
            "XISF lz4 + subblocks",
            xisf_lz4_subblocks(8, 4, 1, &data),
            Streams::Block(2),
            &data,
        ),
        (
            "XISF zlib+sh",
            xisf_shuffled("zlib", 8, 4, 1, &data),
            Streams::WholeImage,
            &data,
        ),
    ];

    for (what, bytes, granularity, planar_levels) in cases {
        let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
        let header = first_image(&mut reader);
        assert_granularity(
            header.granularity(),
            granularity,
            &format!(
                "{what}: the reported granularity is what makes this case the one it claims to be"
            ),
        );
        let len = expected_len(&header);

        let whole = whole_buffer(&mut reader, len);
        let chunked = from_chunks(&mut reader, len);
        assert_same_bits(&chunked, &whole, what);
        let pushed = from_for_each_chunk(&mut reader, len);
        assert_same_bits(&pushed, &whole, &format!("{what}: push form"));

        // Against the pinned form as well, so a delivery defect shared by both paths — which
        // is what tier 2 driving tier 3 makes possible — cannot pass.
        let want: Vec<f32> = planar_levels.iter().map(|l| want_u16(*l)).collect();
        assert_same_bits(&whole, &want, what);
    }
}

/// Criterion *Streaming equals whole-buffer*, push form.
///
/// `ControlFlow::Break` stops delivery without an error, and `next_image()` is still legal
/// afterwards — the reader was left mid-image and skips whatever remains before advancing.
#[test]
fn for_each_chunk_break_stops_delivery_and_next_image_is_still_legal() {
    let data = levels(32);
    let second: Vec<u16> = data.iter().rev().copied().collect();
    let bytes = file(&[
        Hdu::primary()
            .image_2d(16, 8, 4)
            .unsigned_convention(16)
            .data_i16(
                &data
                    .iter()
                    .map(|l| (i32::from(*l) - 32768) as i16)
                    .collect::<Vec<_>>(),
            ),
        Hdu::extension("IMAGE")
            .image_2d(16, 8, 4)
            .card("PCOUNT", "0")
            .card("GCOUNT", "1")
            .unsigned_convention(16)
            .data_i16(
                &second
                    .iter()
                    .map(|l| (i32::from(*l) - 32768) as i16)
                    .collect::<Vec<_>>(),
            ),
    ]);

    let want: Vec<f32> = second.iter().map(|l| want_u16(*l)).collect();

    /// Break on the first chunk, then walk on to the next image.
    fn break_then_advance<S: Source>(reader: &mut Reader<S>, want: &[f32]) {
        first_image(reader);
        let mut delivered = 0usize;
        reader
            .for_each_chunk(|_| {
                delivered += 1;
                ControlFlow::Break(())
            })
            .expect("an early stop is not an error");
        assert_eq!(delivered, 1, "delivery stopped at the first chunk");

        let header = first_image(reader);
        let len = expected_len(&header);
        assert_same_bits(
            &whole_buffer(reader, len),
            want,
            "the image after the break",
        );
        assert!(!reader.next_image().expect("end of source"));
    }

    /// The pull form's equivalent early stop: drop the cursor mid-image, then advance.
    fn drop_then_advance<S: Source>(reader: &mut Reader<S>, want: &[f32]) {
        first_image(reader);
        {
            let mut cursor = reader.chunks();
            cursor
                .next_chunk()
                .expect("a chunk")
                .expect("a first chunk");
        }

        let header = first_image(reader);
        let len = expected_len(&header);
        assert_same_bits(
            &whole_buffer(reader, len),
            want,
            "the image after the dropped cursor",
        );
        assert!(!reader.next_image().expect("end of source"));
    }

    // Both source modes, because the skip that makes `next_image()` legal is a seek on one and
    // a read-and-discard on the other.
    for advance in [break_then_advance, drop_then_advance] {
        advance(
            &mut Reader::seekable(Cursor::new(bytes.clone())).expect("construct"),
            &want,
        );
    }
    for advance in [break_then_advance, drop_then_advance] {
        advance(
            &mut Reader::sequential(Cursor::new(bytes.clone())).expect("construct"),
            &want,
        );
    }
}

// ================================================================== I3: no transformation

/// Criterion *Report-don't-interpret is observable* (invariant I3), FITS half.
///
/// The pattern is asymmetric top to bottom, so a decoder that "helpfully" flipped the rows
/// would fail rather than pass.
#[test]
fn report_dont_interpret_is_observable_fits() {
    // Row 0 is all zero and row 3 is all full-scale: a flip swaps them.
    let stored: Vec<u16> = (0..4)
        .flat_map(|y: u16| (0..4).map(move |x: u16| y * 20000 + x))
        .collect();
    let bytes = file(&[Hdu::primary()
        .image_2d(16, 4, 4)
        .unsigned_convention(16)
        .card("ROWORDER", "'BOTTOM-UP'")
        .data_i16(
            &stored
                .iter()
                .map(|l| (i32::from(*l) - 32768) as i16)
                .collect::<Vec<_>>(),
        )]);

    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    let header = first_image(&mut reader);
    assert_eq!(
        header.row_order(),
        Some(&RowOrder::BottomUp),
        "the keyword is reported verbatim"
    );
    assert_eq!(
        header.keyword("ROWORDER").map(|k| k.value()),
        Some("BOTTOM-UP"),
        "and stays reachable as a keyword"
    );
    // The other half of report-don't-interpret: an accessor whose format does not define the
    // concept reports `None` rather than the other format's default. 72.0 ppi and the identity
    // MTF are XISF's numbers (§11.11, §11.9); reporting them here would attribute them to a
    // format that never stated them, which is indistinguishable to a caller from a FITS frame
    // that did.
    assert_eq!(header.orientation(), None, "FITS defines no orientation");
    assert_eq!(header.resolution(), None, "FITS defines no resolution");
    assert_eq!(
        header.display_function(),
        None,
        "FITS defines no display function"
    );
    assert_eq!(
        header.offset(),
        None,
        "FITS defines no offset; PEDESTAL stays a keyword"
    );

    let got = whole_buffer(&mut reader, expected_len(&header));
    let want: Vec<f32> = stored.iter().map(|l| want_u16(*l)).collect();
    assert_same_bits(&got, &want, "samples arrive in stored order");
}

/// Criterion *Report-don't-interpret is observable*, XISF half.
#[test]
fn report_dont_interpret_is_observable_xisf() {
    let stored: Vec<u16> = (0..4)
        .flat_map(|y: u16| (0..4).map(move |x: u16| y * 20000 + x))
        .collect();
    let template =
        r#"<Image geometry="4:4:1" sampleFormat="UInt16" orientation="180" {loc}/>"#.to_owned();
    let bytes = Unit::new()
        .attached(&template, xisf::le_u16(&stored))
        .build();

    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    let header = first_image(&mut reader);
    assert_eq!(
        header.orientation(),
        Some(&Orientation::Rotate180),
        "the attribute is reported verbatim"
    );
    assert_eq!(
        header.row_order(),
        None,
        "XISF does not have the ROWORDER concept, so no value is fabricated"
    );

    let got = whole_buffer(&mut reader, expected_len(&header));
    let want: Vec<f32> = stored.iter().map(|l| want_u16(*l)).collect();
    assert_same_bits(&got, &want, "samples arrive in stored order, unrotated");
}

/// **Which container a source is, is a fact the crate reports rather than one a caller infers.**
/// Both surfaces answer it: the reader from construction, before any advance, and the header
/// per position. The alternative is an `Option` reported for another purpose —
/// `scaling().is_some()` — which is an inference rather than a report.
#[test]
fn the_container_format_is_reported_by_the_reader_and_by_the_header() {
    let fits = fits_unsigned_u16(4, 4, 1, &levels(16));
    let mut reader = Reader::seekable(Cursor::new(fits)).expect("construct");
    assert_eq!(
        reader.format(),
        Format::Fits,
        "answered from construction, before the first advance"
    );
    let header = first_image(&mut reader);
    assert_eq!(header.format(), Format::Fits);
    assert_eq!(reader.format().as_str(), "FITS");

    let xisf = xisf_uncompressed(4, 4, 1, &levels(16));
    let mut reader = Reader::seekable(Cursor::new(xisf)).expect("construct");
    assert_eq!(reader.format(), Format::Xisf);
    let header = first_image(&mut reader);
    assert_eq!(header.format(), Format::Xisf);
    assert_eq!(header.format().to_string(), "XISF");
}

/// **Seekability is a fact generic code can ask the reader for.** The poison-recovery error
/// tells a caller its move depends on it — retrying an image needs a fresh cursor, which a
/// seekable source allows and a sequential one refuses — and `Source` is a bare marker with
/// nothing to ask, so without this a `fn run<S: Source>(..)` would have to be told.
#[test]
fn the_reader_reports_whether_its_source_can_seek() {
    fn answer<S: Source>(reader: &Reader<S>) -> bool {
        reader.is_seekable()
    }

    let bytes = fits_unsigned_u16(4, 4, 1, &levels(16));
    let seekable = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
    assert!(answer(&seekable));

    let sequential = Reader::sequential(Cursor::new(bytes)).expect("construct");
    assert!(
        !answer(&sequential),
        "a sequential reader refuses to move its cursor backwards, whatever it wraps"
    );
}

/// **The `iter_f64` cursor is a named type carrying the trait set its siblings carry.** Its
/// length is known before the first step and it walks from either end; an
/// `impl Iterator<Item = f64>` return would hide both, and `KeywordIter` and `PropertyIter`
/// are named for exactly that reason.
#[test]
fn iter_f64_is_a_named_exact_size_double_ended_cursor() {
    let owned = Samples::U16(vec![0, 32768, 65535]);
    let mut cursor: IterF64<'_> = owned.as_slice().iter_f64();

    assert_eq!(cursor.len(), 3, "the length is known before the first step");
    assert_eq!(cursor.next(), Some(0.0));
    assert_eq!(cursor.next_back(), Some(65535.0));
    assert_eq!(cursor.len(), 1);
    let cloned: Vec<f64> = cursor.clone().collect();
    assert_eq!(cloned, [32768.0], "Clone forks the cursor where it stands");
    assert_eq!(cursor.next(), Some(32768.0));
    assert_eq!(cursor.next(), None);
    assert_eq!(cursor.next(), None, "and it stays exhausted");
    assert_eq!(cursor.next_back(), None);

    let backwards: Vec<f64> = owned.as_slice().iter_f64().rev().collect();
    assert_eq!(backwards, [65535.0, 32768.0, 0.0]);
}

/// Criterion *`PEDESTAL` and XISF `offset` change no pixel* (invariant I3), FITS half.
#[test]
fn pedestal_changes_no_pixel() {
    let data = levels(32);
    let stored: Vec<i16> = data
        .iter()
        .map(|l| (i32::from(*l) - 32768) as i16)
        .collect();
    let plain = file(&[Hdu::primary()
        .image_2d(16, 8, 4)
        .unsigned_convention(16)
        .data_i16(&stored)]);
    let with_pedestal = file(&[Hdu::primary()
        .image_2d(16, 8, 4)
        .unsigned_convention(16)
        .card("PEDESTAL", "100")
        .data_i16(&stored)]);

    assert_same_bits(
        &decode_first(&with_pedestal),
        &decode_first(&plain),
        "PEDESTAL subtracts nothing",
    );

    let mut reader = Reader::seekable(Cursor::new(with_pedestal)).expect("construct");
    let header = first_image(&mut reader);
    assert_eq!(
        header.keyword("PEDESTAL").map(|k| k.value()),
        Some("100"),
        "and the keyword is retrievable"
    );
}

/// Criterion *`PEDESTAL` and XISF `offset` change no pixel*, XISF half.
#[test]
fn xisf_offset_changes_no_pixel() {
    let data = levels(32);
    let plain = xisf_uncompressed(8, 4, 1, &data);
    let with_offset = Unit::new()
        .attached(
            r#"<Image geometry="8:4:1" sampleFormat="UInt16" offset="100" {loc}/>"#,
            xisf::le_u16(&data),
        )
        .build();

    assert_same_bits(
        &decode_first(&with_offset),
        &decode_first(&plain),
        "offset subtracts nothing",
    );

    let mut reader = Reader::seekable(Cursor::new(with_offset)).expect("construct");
    let header = first_image(&mut reader);
    assert_eq!(
        header.offset(),
        Some(100.0),
        "and the attribute is retrievable"
    );
    let plain_header = {
        let mut r = Reader::seekable(Cursor::new(plain)).expect("construct");
        first_image(&mut r)
    };
    assert_eq!(
        plain_header.offset(),
        Some(0.0),
        "§11.5.2's default is reported when the attribute is absent"
    );
}

// ================================================================== keywords

/// Criterion *Keyword lookup does not case-fold*.
///
/// The latitude carries two fractional digits deliberately: a CI grep fails on a coordinate
/// keyword carrying five or more, which is how a real observatory coordinate is spotted.
#[test]
fn keyword_lookup_does_not_case_fold() {
    let bytes = file(&[Hdu::primary()
        .image_2d(16, 4, 2)
        .unsigned_convention(16)
        .card("SITELAT", "12.34")
        .commentary("HISTORY", "first history")
        .commentary("COMMENT", "first comment")
        .commentary("HISTORY", "second history")
        .commentary("COMMENT", "second comment")
        .data_i16(&[0i16; 8])]);

    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    let header = first_image(&mut reader);

    assert_eq!(header.keyword("SITELAT").map(|k| k.value()), Some("12.34"));
    assert!(
        header.keyword("sitelat").is_none(),
        "names are matched exactly as the file wrote them"
    );

    let history: Vec<&str> = header
        .keywords()
        .iter()
        .filter(|k| k.name() == "HISTORY")
        .map(|k| k.comment().unwrap_or(""))
        .collect();
    assert_eq!(
        history,
        ["first history", "second history"],
        "every duplicate is retrievable, in document order"
    );
    let comments: Vec<&str> = header
        .keywords()
        .iter()
        .filter(|k| k.name() == "COMMENT")
        .map(|k| k.comment().unwrap_or(""))
        .collect();
    assert_eq!(comments, ["first comment", "second comment"]);
}

/// Criterion *A keyword reads the same from either container*.
#[test]
fn a_keyword_reads_the_same_from_either_container() {
    let data = levels(8);
    let fits = file(&[Hdu::primary()
        .image_2d(16, 4, 2)
        .unsigned_convention(16)
        .card_with_comment("DATE-OBS", "'2012-03-15T02:55:15'", "the observation")
        .card("EXPTIME", "120.0")
        .commentary("HISTORY", "stacked from 12 subs")
        .commentary("COMMENT", "a note the writer left")
        .data_i16(
            &data
                .iter()
                .map(|l| (i32::from(*l) - 32768) as i16)
                .collect::<Vec<_>>(),
        )]);
    // §11.6.1's own example spells the value with its FITS quoting intact.
    let xisf_bytes = Unit::new()
        .attached(
            r#"<Image geometry="4:2:1" sampleFormat="UInt16" {loc}>
                 <FITSKeyword name="DATE-OBS" value="'2012-03-15T02:55:15'" comment="the observation"/>
                 <FITSKeyword name="EXPTIME" value="120.0" comment=""/>
                 <FITSKeyword name="HISTORY" value="" comment="stacked from 12 subs"/>
                 <FITSKeyword name="COMMENT" value="" comment="a note the writer left"/>
               </Image>"#,
            xisf::le_u16(&data),
        )
        .build();

    let fits_header = {
        let mut r = Reader::seekable(Cursor::new(fits)).expect("construct");
        first_image(&mut r)
    };
    let xisf_header = {
        let mut r = Reader::seekable(Cursor::new(xisf_bytes)).expect("construct");
        first_image(&mut r)
    };

    for name in ["DATE-OBS", "EXPTIME"] {
        let a = fits_header.keyword(name).expect("the FITS card").value();
        let b = xisf_header.keyword(name).expect("the XISF keyword").value();
        assert_eq!(a, b, "{name} reads byte-identically from either container");
    }
    assert_eq!(
        fits_header.keyword("DATE-OBS").map(|k| k.value()),
        Some("2012-03-15T02:55:15"),
        "the FITS quoting is removed on both surfaces, not carried through on one"
    );

    // Commentary text lands in the same field for both formats.
    for name in ["HISTORY", "COMMENT"] {
        let a = fits_header.keyword(name).expect("the FITS card");
        let b = xisf_header.keyword(name).expect("the XISF keyword");
        assert_eq!(
            a.value(),
            "",
            "{name} carries an empty value by specification"
        );
        assert_eq!(
            b.value(),
            "",
            "{name} carries an empty value by specification"
        );
        assert_eq!(
            a.comment(),
            b.comment(),
            "{name} text lands in the same field for both formats"
        );
    }
}

/// Criterion *`CONTINUE` and `HIERARCH` fold*, on both surfaces, with both §4.2.1.2 edge
/// cases graded alongside.
#[test]
fn continue_and_hierarch_fold_on_both_surfaces() {
    let data = levels(8);
    let fits = file(&[Hdu::primary()
        .image_2d(16, 4, 2)
        .unsigned_convention(16)
        .raw(b"LONGSTR = 'the first part &'")
        .raw(b"CONTINUE  'and the last part'")
        .raw(b"HIERARCH ESO DET EXP = 'twelve'")
        // Edge case: a value ending in `&` with no conforming CONTINUE after it.
        .raw(b"DANGLING= 'keeps its ampersand &'")
        .card("BLOCKER", "1")
        // Edge case: an orphaned CONTINUE, reported as commentary text.
        .raw(b"CONTINUE  'orphaned text'")
        // Edge case: a chain still open when the records run out. The assembled value is
        // what it accumulated -- dropping it would lose the continuation's text silently.
        .raw(b"TRAILING= 'first half &'")
        .raw(b"CONTINUE  'second half &'")
        .data_i16(
            &data
                .iter()
                .map(|l| (i32::from(*l) - 32768) as i16)
                .collect::<Vec<_>>(),
        )]);
    let xisf_bytes = Unit::new()
        .attached(
            r#"<Image geometry="4:2:1" sampleFormat="UInt16" {loc}>
                 <FITSKeyword name="LONGSTR" value="'the first part &amp;'" comment=""/>
                 <FITSKeyword name="CONTINUE" value="'and the last part'" comment=""/>
                 <FITSKeyword name="HIERARCH ESO DET EXP" value="'twelve'" comment=""/>
                 <FITSKeyword name="DANGLING" value="'keeps its ampersand &amp;'" comment=""/>
                 <FITSKeyword name="BLOCKER" value="1" comment=""/>
                 <FITSKeyword name="CONTINUE" value="'orphaned text'" comment=""/>
                 <FITSKeyword name="TRAILING" value="'first half &amp;'" comment=""/>
                 <FITSKeyword name="CONTINUE" value="'second half &amp;'" comment=""/>
               </Image>"#,
            xisf::le_u16(&data),
        )
        .build();

    for (what, bytes) in [("FITS", fits), ("XISF", xisf_bytes)] {
        let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
        let header = first_image(&mut reader);

        assert_eq!(
            header.keyword("LONGSTR").map(|k| k.value()),
            Some("the first part and the last part"),
            "{what}: the chain assembles with no trailing ampersand"
        );
        // A chain left open when the records run out still reports what it assembled, and
        // reports it identically on both surfaces. XISF dropped the continuation entirely --
        // value and record both -- because only the FITS loop had a trailing close.
        assert_eq!(
            header.keyword("TRAILING").map(|k| k.value()),
            Some("first half second half &"),
            "{what}: a chain open at the end assembles from what it accumulated"
        );
        assert_eq!(
            header.keyword("ESO DET EXP").map(|k| k.value()),
            Some("twelve"),
            "{what}: a HIERARCH card answers to its full multi-word name"
        );
        assert!(
            header.keyword("HIERARCH").is_none(),
            "{what}: and not to the bare HIERARCH"
        );
        assert_eq!(
            header.keyword("DANGLING").map(|k| k.value()),
            Some("keeps its ampersand &"),
            "{what}: no conforming CONTINUE follows, so the ampersand is a literal character"
        );
        let orphan = header
            .keywords()
            .iter()
            .find(|k| k.name() == "CONTINUE")
            .expect("the orphaned record is reported");
        assert_eq!(
            orphan.value(),
            "",
            "{what}: an orphaned CONTINUE is commentary, so it carries no value"
        );
        assert!(
            orphan.comment().unwrap_or("").contains("orphaned text"),
            "{what}: its text is preserved as commentary rather than corrupting a value"
        );
    }
}

// ================================================================== tier 1

/// Criterion *Header-only decode reads no pixel bytes* — the check that tier 1 is real rather
/// than nominal.
#[test]
fn header_only_decode_reads_no_pixel_bytes() {
    let data = levels(32);

    let fits = fits_unsigned_u16(8, 4, 1, &data);
    let want_fits = fits_header_region_len(&fits);
    let (counting, counter) = common::CountingRead::new(Cursor::new(fits));
    let reader = Reader::sequential(counting).expect("construct");
    assert_eq!(
        counter.get(),
        want_fits,
        "FITS: construction reads the header region and not one byte more"
    );
    assert!(reader.header().is_none(), "construction selects no image");

    let unit = Unit::new().image_u16(8, 4, 1, &data);
    let want_xisf = u64::from(common::xisf::PREAMBLE as u32) + u64::from(unit.header_length());
    let (counting, counter) = common::CountingRead::new(Cursor::new(unit.build()));
    let reader = Reader::sequential(counting).expect("construct");
    assert_eq!(
        counter.get(),
        want_xisf,
        "XISF: the preamble plus the declared header length, and nothing of the attachment"
    );
    assert!(reader.header().is_none(), "construction selects no image");
}

/// The FITS header region's actual size: 2880-byte blocks up to and including the one carrying
/// `END`. Derived from the fixture rather than assumed, so the assertion stays true if the
/// fixture grows a card.
fn fits_header_region_len(bytes: &[u8]) -> u64 {
    let mut block = 0usize;
    loop {
        let start = block * common::BLOCK;
        let region = &bytes[start..start + common::BLOCK];
        if region.chunks(80).any(|c| c.starts_with(b"END ")) {
            return ((block + 1) * common::BLOCK) as u64;
        }
        block += 1;
    }
}

// ================================================================== no normalized output

/// Criterion *FITS float frames decode natively and refuse normalized output*.
#[test]
fn fits_float_frames_decode_natively_and_refuse_normalized_output() {
    let samples: Vec<f32> = vec![-1.0, 0.0, 0.5, 1.0, 2.0, 0.25, 0.75, 1.5];
    let bytes = file(&[Hdu::primary().image_2d(-32, 4, 2).data_f32(&samples)]);

    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    let header = first_image(&mut reader);
    assert!(
        matches!(header.bounds(), Bounds::Unavailable(_)),
        "FITS defines no representable range for floats"
    );

    // Layer 1 is unaffected: a frame that cannot be normalized is not a frame that cannot be
    // read.
    let mut native = Samples::zeroed(header.sample_format().expect("BITPIX -32"), samples.len());
    reader
        .read_samples_into(&mut native)
        .expect("native samples decode");
    let Samples::F32(decoded) = &native else {
        panic!("BITPIX -32 decodes as F32, got {:?}", native.format());
    };
    // By bits rather than by `==`: the fixture carries `0.0`, and `==` is precisely what would
    // wave a decoder emitting `-0.0` there through.
    assert_same_bits(decoded, &samples, "native f32 samples");

    // A second reader for the normalized half: `read_samples_into` above has already begun the
    // pixel phase, from which `set_bounds` is `InvalidRequest` per *Phases, and what resets*.
    let bytes = file(&[Hdu::primary().image_2d(-32, 4, 2).data_f32(&samples)]);
    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
    first_image(&mut reader);

    let refused = reader
        .read_image_into(&mut vec![0.0f32; samples.len()])
        .expect_err("no representable range");
    assert_eq!(
        kind(&refused),
        "Unsupported",
        "the format defines no default, which § Errors classes as Unsupported"
    );

    // A refused normalized decode reads no pixel byte, so the reader is still configurable —
    // which is what makes `set_bounds` the escape hatch the design calls it.
    reader.set_bounds(0.0, 1.0).expect("a valid range");
    let got = whole_buffer(&mut reader, samples.len());
    // k = 1/(1-0) is exactly 1.0, so the range map is an identity multiply and the clamp is
    // the only thing that happens — which is what makes the two saturated samples the point.
    let want: Vec<f32> = vec![0.0, 0.0, 0.5, 1.0, 1.0, 0.25, 0.75, 1.0];
    assert_same_bits(&got, &want, "set_bounds supplies the missing range");
    assert!(
        !got[0].is_sign_negative(),
        "a sample below lo saturates to +0.0, never -0.0"
    );
}

/// The other *no normalized output* row: an integer `BITPIX` whose `BSCALE`/`BZERO` sit
/// outside the FITS unsigned convention.
#[test]
fn fits_integer_scaling_outside_the_unsigned_convention_refuses_normalized_output() {
    // A genuinely signed frame, and the signed-byte convention. Both decode natively.
    let signed16 = file(&[Hdu::primary()
        .image_2d(16, 4, 2)
        .card("BSCALE", "1")
        .card("BZERO", "0")
        .data_i16(&[-32768, -1, 0, 1, 100, 257, 32767, 12])]);
    let signed8 = file(&[Hdu::primary()
        .image_2d(8, 4, 2)
        .card("BSCALE", "1")
        .card("BZERO", "-128")
        .data_u8(&[0, 1, 2, 3, 128, 129, 254, 255])]);

    for (what, bytes, native) in [
        (
            "BITPIX = 16, BZERO = 0",
            signed16,
            Samples::I16(vec![-32768, -1, 0, 1, 100, 257, 32767, 12]),
        ),
        (
            "BITPIX = 8, BZERO = -128",
            signed8,
            Samples::U8(vec![0, 1, 2, 3, 128, 129, 254, 255]),
        ),
    ] {
        let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
        let header = first_image(&mut reader);
        assert!(
            matches!(header.bounds(), Bounds::Unavailable(_)),
            "{what}: the physical values do not occupy [0, 2^n - 1]"
        );

        let mut dst = Samples::zeroed(header.sample_format().expect("a standard BITPIX"), 8);
        reader
            .read_samples_into(&mut dst)
            .expect("native samples decode");
        assert_eq!(dst, native, "{what}: layer 1 is unaffected");

        // A second reader for the normalized half: the native decode above has begun the pixel
        // phase, from which `set_bounds` is `InvalidRequest`.
        let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
        first_image(&mut reader);
        let refused = reader
            .read_image_into(&mut [0.0f32; 8])
            .expect_err("no representable range");
        assert_eq!(kind(&refused), "Unsupported", "{what}");

        reader.set_bounds(0.0, 255.0).expect("a valid range");
        reader
            .read_image_into(&mut [0.0f32; 8])
            .expect("set_bounds is the escape hatch");
    }
}

// ================================================================== sources and images

/// Criterion *Multi-image and source-mode behaviour*, source-mode half: `open`, `sequential`
/// and `seekable` yield `to_bits()`-identical buffers, for both formats.
#[test]
fn every_source_mode_decodes_the_same_bits() {
    let data = levels(32);
    for (what, bytes) in [
        ("FITS", fits_unsigned_u16(8, 4, 1, &data)),
        ("XISF", xisf_uncompressed(8, 4, 1, &data)),
    ] {
        let seekable = decode_first(&bytes);

        let mut reader = Reader::sequential(Cursor::new(bytes.clone())).expect("construct");
        let header = first_image(&mut reader);
        let len = expected_len(&header);
        let sequential = whole_buffer(&mut reader, len);

        let path = temp_path(&format!("source-mode-{what}"));
        std::fs::write(&path, &bytes).expect("write the fixture");
        let opened = {
            let mut reader = Reader::open(&path).expect("open the fixture");
            let header = first_image(&mut reader);
            whole_buffer(&mut reader, expected_len(&header))
        };
        std::fs::remove_file(&path).ok();

        assert_same_bits(&sequential, &seekable, &format!("{what}: sequential"));
        assert_same_bits(&opened, &seekable, &format!("{what}: open"));
    }
}

/// Criterion *Multi-image and source-mode behaviour*, reset half.
///
/// The reset has to change the decoded **pixels**, not merely the reported header: carrying an
/// override onto the next image is exactly the defect the per-image rule prevents.
#[test]
fn set_bounds_and_select_channel_reset_across_next_image() {
    let data = levels(24);
    let one = r#"<Image geometry="4:2:3" sampleFormat="UInt16" colorSpace="RGB" {loc}/>"#;
    let bytes = Unit::new()
        .attached(one, xisf::le_u16(&data))
        .attached(one, xisf::le_u16(&data))
        .build();

    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");

    first_image(&mut reader);
    reader.select_channel(1).expect("a channel the file has");
    reader.set_bounds(0.0, 32767.0).expect("a valid range");
    let narrowed = reader.header().expect("configured header");
    assert_eq!(narrowed.channels(), Some(1));
    assert_eq!(narrowed.channel_index(), Some(1));
    let first = whole_buffer(&mut reader, 8);

    assert!(reader.next_image().expect("a second image"));
    let header = reader.header().expect("header");
    assert_eq!(
        header.channels(),
        Some(3),
        "select_channel is per-image and was cleared"
    );
    assert_eq!(header.channel_index(), None, "and so is its report");
    assert!(
        matches!(header.bounds(), Bounds::FormatDefault(r) if r.lo() == 0.0 && r.hi() == 65535.0),
        "set_bounds was cleared too, so the format default is back in force"
    );

    let second = whole_buffer(&mut reader, 24);
    let want: Vec<f32> = data.iter().map(|l| want_u16(*l)).collect();
    assert_same_bits(&second, &want, "the second image decodes under the default");

    // The same channel, under the two ranges, differs in the pixels rather than only in the
    // header — which is what makes the reset observable.
    let mut differs = false;
    for (a, b) in first.iter().zip(&second[8..16]) {
        if a.to_bits() != b.to_bits() {
            differs = true;
        }
    }
    assert!(
        differs,
        "the cleared set_bounds must change decoded pixels, not just the reported range"
    );
}

// ================================================================== select_channel

/// Criterion *`select_channel` decodes the same bits as slicing a full decode*.
///
/// Run for both `Planar` and `Normal` storage: the interleaved path is a transposition, and a
/// transposition is where this silently corrupts.
#[test]
fn select_channel_decodes_the_same_bits_as_slicing_a_full_decode() {
    let (width, height, channels) = (4u32, 3u32, 3u32);
    let planar_levels = levels((width * height * channels) as usize);
    let want: Vec<f32> = planar_levels.iter().map(|l| want_u16(*l)).collect();

    for (what, bytes, storage) in [
        (
            "XISF Planar",
            xisf_planar(width, height, channels, &planar_levels),
            Some(PixelStorage::Planar),
        ),
        (
            "XISF Normal",
            xisf_interleaved(width, height, channels, &planar_levels),
            Some(PixelStorage::Normal),
        ),
        (
            // A compressed, shuffled block is materialized whole however few channels the
            // caller asked for, so narrowing there is an extraction out of a buffer the
            // uncompressed path never builds — a second place the same criterion can break.
            "XISF zlib+sh, Planar",
            xisf_shuffled("zlib", width, height, channels, &planar_levels),
            Some(PixelStorage::Planar),
        ),
        (
            "FITS NAXIS = 3",
            fits_unsigned_u16(width, height, channels, &planar_levels),
            Some(PixelStorage::Planar),
        ),
    ] {
        let full = {
            let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
            let header = first_image(&mut reader);
            assert_eq!(header.pixel_storage(), storage, "{what}");
            assert_eq!(header.channels(), Some(channels), "{what}");
            reader.read_image().expect("an unnarrowed decode")
        };
        // Against the independently written expectation, so an interleaved decode that
        // transposes consistently but wrongly cannot agree with itself and pass.
        assert_same_bits(full.samples(), &want, &format!("{what}: full decode"));

        for k in 0..channels {
            let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
            first_image(&mut reader);
            reader.select_channel(k).expect("a channel the file has");
            let header = reader.header().expect("the narrowed header");
            assert_eq!(header.channels(), Some(1), "{what}: narrowed geometry");
            assert_eq!(header.channel_index(), Some(k), "{what}: the file's index");

            let plane = (width * height) as usize;
            let narrowed = whole_buffer(&mut reader, plane);
            // `Image::channel` indexes the *image*, so an unnarrowed decode's channel k is the
            // slice a reader narrowed to file channel k produces.
            assert_same_bits(
                &narrowed,
                full.channel(k).expect("a channel of the full decode"),
                &format!("{what}: channel {k}"),
            );

            // The same narrowing on a sequential source, where dropping the unwanted channels
            // is a forward read-and-discard rather than a seek — a different code path over
            // the same contract.
            let mut reader = Reader::sequential(Cursor::new(bytes.clone())).expect("construct");
            first_image(&mut reader);
            reader.select_channel(k).expect("a channel the file has");
            assert_same_bits(
                &whole_buffer(&mut reader, plane),
                &narrowed,
                &format!("{what}: channel {k}, sequential"),
            );
        }
    }
}

/// The two numbering schemes, pinned: a chunk from a narrowed reader reports the **file's**
/// channel index and a destination range in the **narrowed** buffer's coordinates.
#[test]
fn a_narrowed_chunk_reports_the_file_index_and_narrowed_destination_coordinates() {
    let (width, height, channels) = (4u32, 3u32, 3u32);
    let planar_levels = levels((width * height * channels) as usize);
    let plane = (width * height) as usize;
    let want: Vec<f32> = planar_levels.iter().map(|l| want_u16(*l)).collect();

    // All three layouts, because the file-to-destination mapping under narrowing differs in
    // each: a contiguous run for Planar, a stride for Normal, a data unit for FITS.
    for (what, bytes) in [
        (
            "XISF Planar",
            xisf_planar(width, height, channels, &planar_levels),
        ),
        (
            "XISF Normal",
            xisf_interleaved(width, height, channels, &planar_levels),
        ),
        (
            "FITS NAXIS = 3",
            fits_unsigned_u16(width, height, channels, &planar_levels),
        ),
    ] {
        for k in 0..channels {
            let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
            first_image(&mut reader);
            reader.select_channel(k).expect("a channel the file has");

            let mut assembled = vec![f32::NAN; plane];
            assert_eq!(
                reader.destination_len().expect("the narrowed destination"),
                plane,
                "{what}: the reader sizes the narrowed destination itself"
            );
            let n = reader.normalizer().expect("the fixture has a range");
            let mut seen = 0usize;
            let mut cursor = reader.chunks();
            while let Some(chunk) = cursor.next_chunk().expect("a chunk") {
                assert_eq!(
                    chunk.channel(),
                    k,
                    "{what}: the chunk reports the file's index, never a renumbered zero"
                );
                let range = chunk.range();
                assert!(
                    range.end <= plane,
                    "{what}: the range is in the narrowed buffer's coordinates, not the file's: {range:?}"
                );
                seen += range.len();
                chunk.normalize_into(&n, &mut assembled[range]);
            }
            assert_eq!(
                seen, plane,
                "{what}: the chunks cover the narrowed destination exactly once"
            );

            let start = k as usize * plane;
            assert_same_bits(
                &assembled,
                &want[start..start + plane],
                &format!("{what}: channel {k} assembled from narrowed chunks"),
            );

            // Invariant I2 under narrowing: the same reader's whole-buffer path agrees with
            // the chunks it is built on, which is where a destination-coordinate error that
            // both paths share would still show up against the pinned form above.
            let mut whole = vec![f32::NAN; plane];
            reader
                .read_image_into(&mut whole)
                .expect("narrowed whole-image decode");
            assert_same_bits(
                &whole,
                &assembled,
                &format!("{what}: channel {k}, whole buffer against chunks"),
            );
        }
    }
}
