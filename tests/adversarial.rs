//! Criterion *Adversarial suite: the prior art's cases, ported one for one*.
//!
//! This is not a list invented here. It is a prior XISF decoder's adversarial suite, whose
//! table of **35 named cases** is the accumulated record of what malformed XISF actually
//! looks like, ported one case at a time. **Every case keeps its original name** — spelled
//! exactly, as the string literal each test passes to its grader.
//!
//! Three things make the port non-mechanical, and each is worth stating before the table:
//!
//! 1. **The class comes from this design, never from the original case name.** That decoder
//!    classifies a bad `pixelStorage`/`byteOrder`, a header-length trip and an image-count
//!    trip as corrupt-data, and byte-cap and geometry-overflow trips as bad-geometry. Here
//!    caps are [`Error::LimitExceeded`], malformed enumerations are [`Error::Malformed`], and
//!    declined-but-valid features are [`Error::Unsupported`]. Every class below is derived
//!    from § Errors' three skip-class rules and § Errors → Validation order.
//! 2. **Eleven cases need new fixture bytes**, because this design moves `bounds` and the caps
//!    out of the header phase and decodes three colour-space combinations the prior decoder
//!    rejects. Four of them assert a *decode* rather than an error (the three colour-space
//!    cases and `shuffle item size mismatch`), and a fifth — `no image element` — asserts an
//!    empty walk. Each rebuilt fixture keeps its case name and its attribute combination,
//!    which is what is being diffed, and adds a minimal valid attachment with expected pixels.
//! 3. **Every other case asserts both the expected class and that no value is returned
//!    alongside the error.** [`expect_class`] is where the second half lives: an `Ok` fails
//!    the case by printing the image the decoder should not have produced.
//!
//! **Two departures from a literal byte port, both forced and both narrow.** Three original headers
//! (`wrong root element`, `malformed xml`, `no image element`) are shorter than the 65 bytes
//! §9.2's own validity check requires — `headerLength >= 65` — which the prior decoder does not
//! enforce and this one does. [`assemble`] pads them with trailing spaces *after* the root
//! element, bytes the parser never reaches because it stops at the root's end, so each case
//! still fails for the reason it names rather than on the minimum. And every attachment-
//! bearing fixture is built through `tests/common/xisf.rs`'s builder rather than through a
//! transliteration of the original helper, because that helper iterates the attachment offset to a
//! fixed point **without asserting convergence** and silently gives up — the trap § Testing
//! mechanics names. The attribute combination and the block bytes are what port.
//!
//! Cases the original corpus has no reason to cover are added at the end. The FITS-side malformed
//! cases and the tile-compressed decline are **not** here: they are already graded in
//! `tests/fits_declines.rs`.

#![forbid(unsafe_code)]

mod common;

use std::io::Cursor;

use astroframe::{Error, Image, Limits, Reader, SampleFormat, Samples};

use common::FailingRead;
use common::xisf::{
    Unit, checksum_attr, le_f32, le_u16, lz4, raw_unit, repeating_u16, shuffle, zlib,
};

// ------------------------------------------------------------------ grading

/// The error's variant name. Assertions name the variant rather than matching on a message,
/// which is text the crate is free to reword; a message substring is asserted separately
/// where it is load-bearing.
fn kind(e: &Error) -> &'static str {
    match e {
        Error::Io(_) => "Io",
        Error::Malformed(_) => "Malformed",
        Error::Unsupported(_) => "Unsupported",
        Error::ChecksumMismatch(_) => "ChecksumMismatch",
        Error::LimitExceeded(_) => "LimitExceeded",
        Error::InvalidRequest(_) => "InvalidRequest",
        other => panic!("a variant this suite does not know: {other:?}"),
    }
}

/// The whole decode the original corpus's entry point drives: construct, advance to the first
/// image, decode it. The first error wins, wherever in that sequence it surfaces — which is
/// the point, since this design moves several of these faults from construction to a declined
/// position and two of them into the pixel phase.
fn decode(bytes: &[u8]) -> astroframe::Result<Image> {
    decode_with(bytes, Limits::default())
}

fn decode_with(bytes: &[u8], limits: Limits) -> astroframe::Result<Image> {
    let mut reader = Reader::seekable_with_limits(Cursor::new(bytes), limits)?;
    assert!(
        reader.next_image()?,
        "the fixture must reach an image position; only `no image element` walks empty, and \
         it is graded on its own"
    );
    reader.read_image()
}

/// Grade one ported case: the class § Errors derives for it, **and no value alongside the
/// error**. The `Ok` arm is the second assertion — a decoder that returns an image here fails
/// the case by printing what it produced.
fn expect_class(case: &str, bytes: &[u8], want: &'static str) -> Error {
    expect_class_with(case, bytes, Limits::default(), want)
}

fn expect_class_with(case: &str, bytes: &[u8], limits: Limits, want: &'static str) -> Error {
    match decode_with(bytes, limits) {
        Ok(image) => panic!(
            "{case}: expected {want} and no value; got a {}x{}x{} image of {} samples",
            image.width(),
            image.height(),
            image.channels(),
            image.samples().len()
        ),
        Err(e) => {
            assert_eq!(kind(&e), want, "{case}: {e}");
            e
        }
    }
}

/// Assert a message substring, for the cases where the message is what distinguishes the
/// right check from a different one of the same class.
fn assert_names(case: &str, e: &Error, fragment: &str) {
    let text = e.to_string();
    assert!(
        text.contains(fragment),
        "{case}: the error should name {fragment:?}, got {text:?}"
    );
}

/// Compare by `to_bits()`, never `==`: `==` accepts a sign-of-zero difference, which is
/// exactly the class of defect these comparisons exist to catch.
fn assert_same_bits(case: &str, got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "{case}: sample count");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(
            g.to_bits(),
            w.to_bits(),
            "{case}: sample {i}: got {g:?} ({:#010x}), want {w:?} ({:#010x})",
            g.to_bits(),
            w.to_bits()
        );
    }
}

/// The pinned normalization form for a `UInt16` image at the format default range, written
/// out longhand so an expectation never comes from the code under test.
fn expected_u16(levels: &[u16]) -> Vec<f32> {
    levels
        .iter()
        .map(|&l| l as f32 * (1.0f32 / 65535.0f32))
        .collect()
}

// ------------------------------------------------------------------ fixtures

/// §9.2's own validity check on an XISF header.
const MIN_HEADER: usize = 65;

/// The original corpus's `assemble`, plus the §9.2 minimum-length padding described in the module
/// documentation.
fn assemble(header: &str, trailing: &[u8]) -> Vec<u8> {
    let mut header = header.to_owned();
    while header.len() < MIN_HEADER {
        header.push(' ');
    }
    let declared = u32::try_from(header.len()).expect("a fixture header fits u32");
    raw_unit(b"XISF0100", declared, &header, trailing)
}

/// The original corpus's `rawFile`: a bare `<Image>` tag, attributes only, wrapped in a minimal
/// valid header. Twenty-three of the thirty-five fixtures are this shape, and eighteen of
/// those carry a fault this design reaches **before** it looks for a `location` — which is
/// the whole reason § Errors → Validation order runs the location check last.
fn raw_image(attrs: &str) -> Vec<u8> {
    Unit::new().xml(&format!("<Image {attrs}></Image>")).build()
}

/// The original corpus's `attachFile`: the given attributes plus a `location` the builder resolves,
/// with the block appended. The builder asserts the offset iteration converges.
fn attach_image(attrs: &str, block: Vec<u8>) -> Vec<u8> {
    Unit::new()
        .attached(&format!("<Image {attrs} {{loc}}></Image>"), block)
        .build()
}

/// The original corpus's known-good baseline, whose bytes two cases mutate: 16 x 16 x 1 `UInt16`,
/// colour space absent and therefore `Gray`.
fn good_file() -> Vec<u8> {
    Unit::new()
        .image_u16(16, 16, 1, &repeating_u16(16 * 16))
        .build()
}

// ==================================================================
// Group 1 — never reach an <Image> element. Bytes port unchanged; each fails at
// construction, so there is no reader to walk.
// ==================================================================

#[test]
fn bad_signature() {
    let mut bytes = b"NOTXISF!".to_vec();
    bytes.extend_from_slice(&[0u8; 32]);
    let e = expect_class("bad signature", &bytes, "Malformed");
    assert_names("bad signature", &e, "XISF0100");
}

#[test]
fn too_short_for_preamble() {
    let e = expect_class("too short for preamble", b"XISF01", "Malformed");
    assert_names("too short for preamble", &e, "8 bytes");
}

#[test]
fn wrong_root_element() {
    let bytes = assemble(r#"<?xml version="1.0"?><notxisf></notxisf>"#, &[]);
    let e = expect_class("wrong root element", &bytes, "Malformed");
    // Load-bearing: padded to the §9.2 minimum, these bytes must fail on the *root element*
    // rather than on the length, which is the same class and a different check.
    assert_names("wrong root element", &e, "notxisf");
}

#[test]
fn malformed_xml() {
    let bytes = assemble(r#"<?xml version="1.0"?><xisf><Image geometry="#, &[]);
    let e = expect_class("malformed xml", &bytes, "Malformed");
    assert_names("malformed xml", &e, "well-formed XML");
}

// ==================================================================
// Group 2 — never reaches an <Image> element, and is not an error here. § Deliberate
// divergences from prior art: the prior decoder refuses a unit carrying no <Image>; this one walks zero images,
// exactly as a FITS primary with NAXIS = 0 and no image extension does.
// ==================================================================

#[test]
fn no_image_element() {
    let bytes = assemble(r#"<?xml version="1.0"?><xisf version="1.0"></xisf>"#, &[]);
    let mut reader = Reader::seekable(Cursor::new(&bytes)).expect("construction succeeds");
    assert!(
        !reader.next_image().expect("advancing succeeds"),
        "no image element: the walk ends normally rather than erroring"
    );
    assert!(
        reader.header().is_none(),
        "no image element: no position, so no header"
    );
}

// ==================================================================
// Group 3 — geometry and sample format, header-only. Bytes port unchanged: geometry is first
// in both validation orders, so the fault is reached before the missing `location`.
// ==================================================================

#[test]
fn geometry_integer_too_large() {
    expect_class(
        "geometry integer too large",
        &raw_image(r#"geometry="99999999999999999999:1:1" sampleFormat="UInt16""#),
        "Malformed",
    );
}

#[test]
fn geometry_non_numeric_field() {
    expect_class(
        "geometry non-numeric field",
        &raw_image(r#"geometry="4:4:abc" sampleFormat="UInt16""#),
        "Malformed",
    );
}

#[test]
fn geometry_non_positive_field() {
    let e = expect_class(
        "geometry non-positive field",
        &raw_image(r#"geometry="4:4:0" sampleFormat="UInt16""#),
        "Malformed",
    );
    // §8.5.1 calls this an empty image and forbids serializing one — a file-validity rule
    // rather than a scope decision, which is why it is Malformed and not Unsupported.
    assert_names("geometry non-positive field", &e, "zero-length axis");
}

/// The original case is `geometry="4:4"`, and this design **splits** it: fewer than two fields is
/// `Malformed`, exactly two is a valid one-dimensional image and `Unsupported`. Only the first
/// says the file is broken, so both halves are graded here rather than the ported half alone.
#[test]
fn geometry_too_few_fields() {
    expect_class(
        "geometry too few fields",
        &raw_image(r#"geometry="4:4" sampleFormat="UInt16""#),
        "Unsupported",
    );
    expect_class(
        "geometry too few fields (one field)",
        &raw_image(r#"geometry="4" sampleFormat="UInt16""#),
        "Malformed",
    );
}

#[test]
fn higher_dimensional_geometry() {
    expect_class(
        "higher-dimensional geometry",
        &raw_image(r#"geometry="4:4:1:1" sampleFormat="UInt16""#),
        "Unsupported",
    );
}

#[test]
fn unsupported_sample_format() {
    expect_class(
        "unsupported sample format",
        &raw_image(r#"geometry="2:2:1" sampleFormat="Complex32""#),
        "Unsupported",
    );
}

// ==================================================================
// Group 4 — colour space, header-only, still an error. Bytes port unchanged.
// ==================================================================

#[test]
fn unsupported_color_space() {
    expect_class(
        "unsupported color space",
        &raw_image(r#"geometry="2:2:3" sampleFormat="UInt16" colorSpace="CIELab""#),
        "Unsupported",
    );
}

// ==================================================================
// Group 5 — colour space, header-only, now a **decode success**. New fixture bytes: the prior
// originals have no `location` and no block, which is adequate for asserting an error and
// useless for asserting a decode. The case names and the attribute combinations are kept —
// that is what is being diffed — and a minimal valid attachment with expected pixels is added.
//
// § Deliberate divergences from prior art: the prior decoder validates channel count against the colour space's
// nominal count. Channels beyond it are alpha channels (§8.5.1) and decode as ordinary
// channels here.
// ==================================================================

/// `colorSpace` absent means `Gray` in **both** decoders — that is not the divergence. The
/// divergence is the validation that follows the default.
#[test]
fn colorspace_absent_with_three_channels() {
    let levels = repeating_u16(2 * 2 * 3);
    let bytes = attach_image(r#"geometry="2:2:3" sampleFormat="UInt16""#, le_u16(&levels));
    let image = decode(&bytes).expect("colorspace absent with three channels decodes");
    assert_eq!((image.width(), image.height(), image.channels()), (2, 2, 3));
    assert_same_bits(
        "colorspace absent with three channels",
        image.samples(),
        &expected_u16(&levels),
    );
}

#[test]
fn gray_with_two_channels() {
    let levels = repeating_u16(2 * 2 * 2);
    let bytes = attach_image(
        r#"geometry="2:2:2" sampleFormat="UInt16" colorSpace="Gray""#,
        le_u16(&levels),
    );
    let image = decode(&bytes).expect("gray with two channels decodes");
    assert_eq!((image.width(), image.height(), image.channels()), (2, 2, 2));
    assert_same_bits(
        "gray with two channels",
        image.samples(),
        &expected_u16(&levels),
    );
}

#[test]
fn rgb_with_four_channels() {
    let levels = repeating_u16(2 * 2 * 4);
    let bytes = attach_image(
        r#"geometry="2:2:4" sampleFormat="UInt16" colorSpace="RGB""#,
        le_u16(&levels),
    );
    let image = decode(&bytes).expect("rgb with four channels decodes");
    assert_eq!((image.width(), image.height(), image.channels()), (2, 2, 4));
    assert_same_bits(
        "rgb with four channels",
        image.samples(),
        &expected_u16(&levels),
    );
}

// ==================================================================
// Group 6 — `bounds`, header-only. New fixture bytes: with no `location` the missing-location
// rule fires first here and the frame is `Malformed` at the declined position, so the intended
// assertion would never run. Rebuilt with a valid location and a real block, they fail at
// `read_image_into` — which is where `bounds` is raised, being evaluated at parse and raised at
// normalize (§ Errors → Validation order).
//
// Each also asserts the half that makes the placement worth having: **native samples still
// decode**. A frame that cannot be normalized is not thereby a frame that cannot be read, and
// without that assertion the fixture would pass for a decoder that simply refused the file.
// ==================================================================

/// The four `Float32` samples every `bounds` fixture stores. Chosen to be exactly
/// representable, so the native-sample comparison is about delivery rather than about
/// rounding.
const BOUNDS_SAMPLES: [f32; 4] = [0.0, 0.25, 0.5, 1.0];

fn bounds_fixture(declared: &str) -> Vec<u8> {
    attach_image(
        &format!(r#"geometry="2:2:1" sampleFormat="Float32" bounds="{declared}""#),
        le_f32(&BOUNDS_SAMPLES),
    )
}

fn grade_bounds(case: &str, declared: &str) {
    let bytes = bounds_fixture(declared);
    let e = expect_class(case, &bytes, "Malformed");
    assert_names(case, &e, "bounds");

    // Native samples still decode: the same file, the same reader, the non-normalizing entry
    // point. This is what keeps the case from passing for a decoder that refuses the frame
    // outright.
    let mut reader = Reader::seekable(Cursor::new(&bytes)).expect("construction succeeds");
    assert!(reader.next_image().expect("advance"));
    let mut native = Samples::zeroed(SampleFormat::F32, 4);
    reader
        .read_samples_into(&mut native)
        .unwrap_or_else(|e| panic!("{case}: native samples must still decode: {e}"));
    let Samples::F32(got) = &native else {
        panic!("{case}: destination format changed")
    };
    assert_same_bits(&format!("{case} (native)"), got, &BOUNDS_SAMPLES);
}

#[test]
fn bounds_equal() {
    grade_bounds("bounds equal", "1:1");
}

#[test]
fn bounds_reversed() {
    grade_bounds("bounds reversed", "2:1");
}

#[test]
fn bounds_nan() {
    grade_bounds("bounds nan", "NaN:1");
}

/// The original fixture spells it `Inf`, which §8.3.3 does not admit — the conforming spellings are
/// `+Inf` and `-Inf`. Both spellings land in the same place here: an unparseable field and a
/// parsed infinity alike fail the range validity rule on `1.0f32 / ((hi - lo) as f32)`. Both
/// are graded, so the case does not depend on which one the corpus happened to write.
#[test]
fn bounds_positive_infinity() {
    grade_bounds("bounds positive infinity", "0:Inf");
    grade_bounds("bounds positive infinity (conforming spelling)", "0:+Inf");
}

#[test]
fn bounds_negative_infinity() {
    grade_bounds("bounds negative infinity", "-Inf:0");
}

// ==================================================================
// Group 7 — caps. New fixture bytes, for the same reason: the caps run in the **pixel phase**
// here, so a fixture with no `location` never reaches them. Each keeps its case name and its
// geometry and adds a valid location and a real block.
//
// A cap trip is `LimitExceeded` here; the prior decoder called two of these bad-geometry.
// ==================================================================

/// `geometry="100000:100000:1000"` is three positive integers and parses; what that decoder catches is
/// its total-samples ceiling under a class name describing the arithmetic rather than the
/// check. Here the **total-samples cap runs first** in the pixel phase, on the file's declared
/// geometry alone, which is what leaves this assertion determinate rather than arguable either
/// way: with that cap raised the output-byte cap is what answers instead, and the two are
/// distinguished by name below.
#[test]
fn geometry_overflow() {
    let bytes = attach_image(
        r#"geometry="100000:100000:1000" sampleFormat="UInt16""#,
        le_u16(&repeating_u16(4)),
    );
    let e = expect_class("geometry overflow", &bytes, "LimitExceeded");
    assert_names("geometry overflow", &e, "total samples");

    // The other direction, which is what makes the ordering an assertion rather than a
    // coincidence: raise the cap this trips and the *next* cap in the pixel-phase order
    // answers. Neither path sizes a buffer, so nothing here allocates.
    let mut raised = Limits::default();
    raised.total_samples = u64::MAX;
    let e = expect_class_with(
        "geometry overflow (raised)",
        &bytes,
        raised,
        "LimitExceeded",
    );
    assert_names("geometry overflow (raised)", &e, "decoded output bytes");
}

/// The same unchecked-product shape as `geometry_whose_axis_product_exceeds_u64`, two hops
/// further down, and reachable **only once the caller raises the total-samples cap** — which
/// § The caps explicitly contemplates ("a workstation tool wants the output-byte cap raised")
/// and which this suite itself does in three places.
///
/// At `total_samples = u64::MAX` every geometry passes the comparison, so the saturating count
/// flows on to the products that size a staging row, a destination and a sample's offset. Those
/// used to be plain `*`: they panicked under the overflow checks every test and fuzz build
/// carries, and wrapped in a release build into an undersized buffer or an absurd allocation.
/// A raised cap is a caller saying "I will accept a large file", not "I will accept a crash".
#[test]
fn a_geometry_whose_byte_extent_exceeds_u64_under_a_raised_cap() {
    let mut raised = Limits::default();
    raised.total_samples = u64::MAX;
    raised.decoded_output_bytes = u64::MAX;
    raised.materialized_bytes = u64::MAX;

    // width x channels alone nearly fills u64; times eight bytes a sample, it does not fit.
    let bytes = attach_image(
        r#"geometry="4294967295:1:4294967295" pixelStorage="Normal" sampleFormat="UInt64""#,
        le_u16(&repeating_u16(4)),
    );
    let e = expect_class_with(
        "byte extent above u64 under a raised cap",
        &bytes,
        raised,
        "LimitExceeded",
    );
    assert_names(
        "byte extent above u64 under a raised cap",
        &e,
        "total samples",
    );

    // The chunked entry point reaches the staging-row arithmetic by a different route -- it
    // starts the decoder rather than sizing a destination -- so it is gated separately and
    // graded separately. This is the path the defect was first observed on.
    let mut reader =
        Reader::seekable_with_limits(Cursor::new(&bytes), raised).expect("construction succeeds");
    assert!(reader.next_image().expect("the position is reached"));
    let e = reader
        .chunks()
        .next_chunk()
        .expect_err("the chunked path refuses it rather than computing it");
    assert_eq!(kind(&e), "LimitExceeded", "{e}");
    assert_names("byte extent above u64, chunked", &e, "total samples");
}

/// **A failed chunk poisons the image**: pulling again after an error must not hand back
/// pixels, and it did.
///
/// A subblocked LZ4 block whose second subblock is corrupt fails when that boundary is
/// crossed. The failure left `Stream::Plain` in the decoder — the outgoing stream is dropped
/// before the replacement is built, and the replacement's `?` returns with `Plain` in its
/// place — and `Plain` is the *uncompressed* reader, which seeks into the file and hands back
/// whatever is there. A caller that logged the error and kept pulling received the block's own
/// compressed bytes as pixels and then a clean end of image, which is § The organizing
/// principle's "never a best guess" exactly inverted.
///
/// The guard is general rather than aimed at this state: any pixel-phase error leaves a
/// decoder describing something other than the file.
#[test]
fn a_failed_chunk_poisons_the_rest_of_the_image() {
    // Two independently-compressed halves; the second's stored bytes are then overwritten,
    // keeping the declared length so the fault surfaces at decode rather than at parse.
    let levels = repeating_u16(16);
    let plain = le_u16(&levels);
    let (first, second) = plain.split_at(plain.len() / 2);
    let (ca, cb) = (lz4(first), lz4(second));
    let list = format!("{},{}:{},{}", ca.len(), first.len(), cb.len(), second.len());
    let mut stored = ca;
    stored.extend(std::iter::repeat_n(0xFFu8, cb.len()));

    let bytes = attach_image(
        &format!(
            r#"geometry="4:4:1" sampleFormat="UInt16" compression="lz4:{}" subblocks="{list}""#,
            plain.len()
        ),
        stored,
    );

    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construction succeeds");
    assert!(reader.next_image().expect("the walk advances"));
    let mut chunks = reader.chunks();

    let mut before = 0usize;
    let mut after = 0usize;
    let mut ended_cleanly = false;
    let mut first_error = None;
    // Keep pulling past the error, which is what a caller that logs and continues does.
    for _ in 0..8 {
        match chunks.next_chunk() {
            Ok(Some(_)) => {
                if first_error.is_none() {
                    before += 1;
                } else {
                    after += 1;
                }
            }
            Ok(None) => {
                ended_cleanly = first_error.is_some();
                break;
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(kind(&e));
                } else {
                    // Every call after the first error must keep refusing, and refuse as a
                    // caller mistake: the file's fault was already reported once.
                    assert_eq!(kind(&e), "InvalidRequest", "after the first failure: {e}");
                }
            }
        }
    }
    assert_eq!(
        first_error,
        Some("Malformed"),
        "the corrupt subblock is the file's fault"
    );
    // The rows the intact first subblock covers are legitimately delivered before the fault.
    assert!(before > 0, "the intact subblock's rows decode");
    assert_eq!(
        after, 0,
        "no chunk may be delivered after the decode failed"
    );
    assert!(
        !ended_cleanly,
        "a failed image must not go on to report a clean end"
    );
}

/// A `zlib` stream that ends **before** the geometry-implied size, with stored bytes still
/// unread behind it. This hung — not errored, hung — on the commonest streamed codec, under
/// default limits, from a file of well under a hundred bytes.
///
/// The streaming loop's only escape for a non-productive iteration required the *input* to be
/// exhausted. `StreamEnd` reached with bytes still buffered leaves the decompressor consuming
/// nothing and producing nothing while input remains, so that escape never fired and the
/// truncation error the caller is written to raise was never reached. § Errors gives the
/// answer this must produce: the stored bytes ran out before the geometry-implied size, which
/// is `Malformed`.
///
/// Invariant I5 is the reason this is a case rather than a nicety — "no input, however
/// hostile, causes a panic **or a hang**", and a hang is the half no fuzz run of ordinary
/// length reports as anything but a timeout.
#[test]
fn zlib_stream_ending_before_the_geometry_implied_size() {
    let mut stored = zlib(&[1u8, 2, 3, 4]);
    // Trailing filler: this is what keeps the input from ever going dry.
    stored.extend_from_slice(&[0xAA; 10]);
    let bytes = attach_image(
        r#"geometry="8:2:1" sampleFormat="UInt8" compression="zlib:16""#,
        stored,
    );
    let e = expect_class(
        "zlib stream ending before the geometry-implied size",
        &bytes,
        "Malformed",
    );
    assert_names(
        "zlib stream ending before the geometry-implied size",
        &e,
        "truncated",
    );
}

/// The multiplicative middle of the geometry cases, which the two either side of it miss:
/// `geometry integer too large` overflows `u32` while still parsing one axis, and `geometry
/// overflow` reaches only 10^13, comfortably inside `u64`.
///
/// Three `u32` axes reach 2^96. Multiplying them plainly panics wherever overflow checks are
/// on — every debug and test build, and `cargo-fuzz`, which § Fuzzing makes a release blocker
/// — and wraps everywhere else. The wrap is the worse half: a product congruent to something
/// small modulo 2^64 walks straight through the total-samples cap that exists to reject the
/// declaration, and the file is refused several phases later by a size cross-check, as
/// `Malformed` where § Errors → Validation order fixes `LimitExceeded`.
///
/// Both spellings below are about 400 bytes of XISF. The second is the one a wrapping
/// implementation lets through: 2^17 x 2^16 x 2^31 is exactly 2^64.
#[test]
fn geometry_whose_axis_product_exceeds_u64() {
    for (case, geometry) in [
        ("saturating", "4294967295:4294967295:4294967295"),
        ("wrapping to zero", "131072:65536:2147483648"),
    ] {
        let bytes = attach_image(
            &format!(r#"geometry="{geometry}" sampleFormat="UInt16""#),
            le_u16(&repeating_u16(4)),
        );
        let e = expect_class(case, &bytes, "LimitExceeded");
        assert_names(case, &e, "total samples");
    }
}

/// Within the sample ceiling (2^29 < 2^30) but the `f32` output is 2 GiB, over the 2^30
/// output-byte cap. The tiny attachment the header points at is never read: the error comes
/// from the cap, not from the reader — which is the whole of what the prior
/// `TestCapDoesNotAllocate` input contributes, and here it carries the real instrumentation in
/// `tests/bombs.rs`.
///
/// The raised-cap direction is deliberately not run for this one: raising it would allocate
/// the 2 GiB destination the cap exists to refuse. Instead the fixture is shown to be sound
/// apart from the cap — the header parses, declines nothing, and reports the geometry it
/// declared — so the case cannot be passing because the file is broken some other way.
#[test]
fn output_byte_cap_exceeded() {
    let bytes = attach_image(
        r#"geometry="1048576:512:1" sampleFormat="UInt16""#,
        le_u16(&repeating_u16(4)),
    );

    let mut reader = Reader::seekable(Cursor::new(&bytes)).expect("construction succeeds");
    assert!(reader.next_image().expect("advance"));
    let header = reader.header().expect("a header at this position");
    assert!(
        header.decline_reason().is_none(),
        "output byte cap exceeded: the fixture must be sound apart from the cap"
    );
    assert_eq!(
        (header.width(), header.height(), header.channels()),
        (Some(1_048_576), Some(512), Some(1))
    );

    let e = expect_class("output byte cap exceeded", &bytes, "LimitExceeded");
    assert_names("output byte cap exceeded", &e, "decoded output bytes");
}

/// **This case checks invariant I4 directly rather than incidentally.** The 2 GiB the geometry
/// implies for the native sample block is used as a *cross-check and as a cap input*, never as
/// an allocation size: the block is `lz4`, which has no framing and so must be held whole, and
/// the `Materialized bytes` cap refuses that buffer **before it is sized**. Nothing here
/// allocates 2 GiB, or 2 GB, or anything at all beyond the fixture.
///
/// Graded through `chunks()` rather than `read_image()`, because the block's own size is what
/// is under test: `read_image` would first allocate the 2^28-sample `f32` destination, which
/// this frame legitimately permits at exactly the output-byte cap. For the same reason the
/// raised-cap direction is not run — raising `materialized_bytes` would allocate the 2 GiB
/// buffer the cap exists to refuse. The fixture's soundness apart from the cap is asserted
/// instead.
#[test]
fn sample_block_byte_cap_exceeded() {
    const IMPLIED: u64 = 16384 * 16384 * 8;
    let bytes = attach_image(
        &format!(r#"geometry="16384:16384:1" sampleFormat="Float64" compression="lz4:{IMPLIED}""#),
        lz4(&le_u16(&repeating_u16(64))),
    );

    let mut reader = Reader::seekable(Cursor::new(&bytes)).expect("construction succeeds");
    assert!(reader.next_image().expect("advance"));
    let header = reader.header().expect("a header at this position");
    assert!(
        header.decline_reason().is_none(),
        "sample block byte cap exceeded: the fixture must be sound apart from the cap"
    );
    assert_eq!(header.sample_format(), Some(SampleFormat::F64));

    let e = reader
        .chunks()
        .next_chunk()
        .expect_err("sample block byte cap exceeded: expected LimitExceeded and no chunk");
    assert_eq!(
        kind(&e),
        "LimitExceeded",
        "sample block byte cap exceeded: {e}"
    );
    assert_names("sample block byte cap exceeded", &e, "materialized bytes");
}

// ==================================================================
// Group 8 — the twelve that carry a `location`. Bytes port unchanged. Carrying a `location` is
// what these have in common and what carries them past the location check this design runs
// last — not carrying block bytes: four have none behind the location.
// ==================================================================

/// The prior decoder classifies a header-length trip as corrupt-data. Here the declared header
/// length is a **cap** (§ The caps), rejected above it *before the read*, so the class is
/// `LimitExceeded`. 0xFFFFFF is 16 MiB against the 8 MiB default.
#[test]
fn header_length_overruns_file() {
    let mut bytes = good_file();
    bytes[8..12].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
    let e = expect_class("header length overruns file", &bytes, "LimitExceeded");
    assert_names("header length overruns file", &e, "XML header length");

    // The other direction: the same declared length under a cap that admits it fails on the
    // bytes not being there, which is what proves the cap was the check that fired.
    let mut raised = Limits::default();
    raised.xml_header_bytes = 32 << 20;
    let e = expect_class_with(
        "header length overruns file (raised)",
        &bytes,
        raised,
        "Malformed",
    );
    assert_names("header length overruns file (raised)", &e, "truncated");
}

/// § Deliberate divergences from prior art: the prior decoder rejects this; the specification never ties
/// `item-size` to the sample width, so it is over-strict. The bytes port; the assertion flips
/// from an error class to a successful decode with expected pixel values.
#[test]
fn shuffle_item_size_mismatch() {
    // The original fixture compresses 32 zero bytes, whose shuffle is the identity — which would let
    // a decoder that skipped the unshuffle pass. The block here holds the real §10.6.2
    // transform of real samples, so the reverse transform is actually exercised.
    let levels = repeating_u16(16);
    let raw = le_u16(&levels);
    assert_eq!(raw.len(), 32, "the original fixture's 32-byte block");
    let bytes = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="zlib+sh:32:4""#,
        zlib(&shuffle(&raw, 4)),
    );
    let image = decode(&bytes).expect("shuffle item size mismatch decodes");
    assert_same_bits(
        "shuffle item size mismatch",
        image.samples(),
        &expected_u16(&levels),
    );
}

#[test]
fn negative_offset() {
    let e = expect_class(
        "negative offset",
        &raw_image(
            r#"geometry="2:2:1" sampleFormat="UInt16" offset="-1" location="attachment:0:8""#,
        ),
        "Malformed",
    );
    assert_names("negative offset", &e, "offset");
}

/// **Invariant I4, directly.** `compression="zlib:64"` declares an uncompressed size that
/// disagrees with the 32 bytes the geometry implies. The declared number is used as a
/// cross-check and refused *before any buffer is allocated*, which is what makes decompression
/// bombs structurally impossible here rather than merely caught.
#[test]
fn compressed_usize_disagrees_with_geometry() {
    let bytes = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="zlib:64""#,
        zlib(&[0u8; 32]),
    );
    let e = expect_class(
        "compressed usize disagrees with geometry",
        &bytes,
        "Malformed",
    );
    assert_names(
        "compressed usize disagrees with geometry",
        &e,
        "uncompressed size",
    );
}

#[test]
fn uncompressed_attachment_too_large() {
    let bytes = attach_image(r#"geometry="2:2:1" sampleFormat="UInt16""#, vec![0u8; 16]);
    let e = expect_class("uncompressed attachment too large", &bytes, "Malformed");
    assert_names(
        "uncompressed attachment too large",
        &e,
        "geometry implies 8",
    );
}

#[test]
fn zero_size_attachment() {
    let bytes = attach_image(r#"geometry="2:2:1" sampleFormat="UInt16""#, Vec::new());
    let e = expect_class("zero-size attachment", &bytes, "Malformed");
    assert_names("zero-size attachment", &e, "declares 0 bytes");
}

/// An unknown codec is a valid, self-consistent file using something this version declines, so
/// it is `Unsupported`. It reaches the codec check only because `location` is validated before
/// `compression` and after everything else — the ordering § Errors fixes.
#[test]
fn unsupported_codec() {
    let e = expect_class(
        "unsupported codec",
        &raw_image(
            r#"geometry="2:2:1" sampleFormat="UInt16" compression="lzma:8" location="attachment:0:8""#,
        ),
        "Unsupported",
    );
    assert_names("unsupported codec", &e, "lzma");
}

#[test]
fn attachment_out_of_bounds() {
    let e = expect_class(
        "attachment out of bounds",
        &raw_image(r#"geometry="2:2:1" sampleFormat="UInt16" location="attachment:1000000:8""#),
        "Malformed",
    );
    assert_names("attachment out of bounds", &e, "runs past");
}

/// **Invariant I4, directly.** The declared block size is 4 bytes against the 8 the geometry
/// implies. The geometry sizes the buffer and the declared size is only ever cross-checked
/// against it, so the disagreement is an error rather than a four-byte read into an
/// eight-byte expectation.
#[test]
fn uncompressed_block_size_mismatch() {
    let bytes = attach_image(r#"geometry="2:2:1" sampleFormat="UInt16""#, vec![0u8; 4]);
    let e = expect_class("uncompressed block size mismatch", &bytes, "Malformed");
    assert_names("uncompressed block size mismatch", &e, "geometry implies 8");
}

#[test]
fn truncated_mid_block() {
    let whole = good_file();
    let bytes = whole[..whole.len() - 100].to_vec();
    let e = expect_class("truncated mid-block", &bytes, "Malformed");
    assert_names("truncated mid-block", &e, "runs past");
}

/// The geometry implies 32 bytes; the stream inflates to 4 MiB. § Hardening: the block
/// inflates to exactly the expected length and is then **required to be at end of stream**, so
/// the bomb is refused after a few dozen bytes rather than after materializing. The allocation
/// half of this is instrumented in `tests/bombs.rs`.
#[test]
fn zlib_decompression_bomb() {
    let bytes = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="zlib:32""#,
        zlib(&vec![0u8; 4 << 20]),
    );
    let e = expect_class("zlib decompression bomb", &bytes, "Malformed");
    assert_names("zlib decompression bomb", &e, "inflates past");
}

/// The LZ4 half. A bare LZ4 block decompresses only as a whole, into a destination sized from
/// the geometry — so the 8 KiB payload has nowhere to go in the 32 bytes the geometry implies.
#[test]
fn lz4_decompression_bomb() {
    let bytes = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="lz4:32""#,
        lz4(&vec![0u8; 8192]),
    );
    expect_class("lz4 decompression bomb", &bytes, "Malformed");
}

// ==================================================================
// The tests outside the table.
// ==================================================================

/// `TestBombDoesNotAllocate` **measures no allocation**, despite its name: its body asserts
/// only a corrupt-data class on a 64 MiB zlib bomb. What ports is the **case** — the input —
/// and it is graded here for its class. The instrumentation the name implies lives in
/// `tests/bombs.rs`, where the criterion *A decompression bomb is refused without
/// materializing* actually puts it.
#[test]
fn bomb_does_not_allocate_ports_as_a_case() {
    let bytes = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="zlib:32""#,
        zlib(&vec![0u8; 64 << 20]),
    );
    expect_class("TestBombDoesNotAllocate", &bytes, "Malformed");
}

/// `TestCapDoesNotAllocate` likewise measures no allocation: its body asserts only a
/// bad-geometry class on a geometry inside the sample ceiling and past the byte cap. Its input
/// ports; the class is this design's (`LimitExceeded`), and the allocation assertion is in
/// `tests/bombs.rs`.
#[test]
fn cap_does_not_allocate_ports_as_a_case() {
    let bytes = attach_image(
        r#"geometry="1048576:512:1" sampleFormat="UInt16""#,
        vec![0u8; 8],
    );
    let e = expect_class("TestCapDoesNotAllocate", &bytes, "LimitExceeded");
    assert_names("TestCapDoesNotAllocate", &e, "decoded output bytes");
}

/// `TestIOErrorClassified` is the one of the three that asserts what it claims, and it ports
/// as it stands: a reader failing mid-read rather than at end of file is `Io` — **abort**, not
/// skip — with the underlying cause preserved in the message. Truncation is deliberately not
/// this: a short file is bad data, not a failing disk.
#[test]
fn io_error_classified() {
    let bytes = good_file();
    let source = FailingRead {
        data: bytes,
        pos: 0,
        // The 8-byte signature read succeeds; the read of the header-length field does not.
        ok_bytes: 8,
    };
    // `FailingRead` is not `Debug`, so the "no value alongside the error" half is spelled out
    // rather than delegated to `expect_err`.
    let e = match Reader::sequential(source) {
        Ok(_) => panic!("TestIOErrorClassified: expected Io and no reader, got a reader"),
        Err(e) => e,
    };
    assert_eq!(kind(&e), "Io", "TestIOErrorClassified: {e}");
    assert!(e.is_io(), "TestIOErrorClassified: the skip/abort split");
    assert_names("TestIOErrorClassified", &e, "simulated device failure");
}

/// `TestImageCountCap` ports with its class re-derived: the prior decoder calls an image-count
/// trip corrupt-data, and here it is a cap and therefore `LimitExceeded`. It also moves
/// where it surfaces — the cap counts `next_image()` **advances**, so it is the advance past
/// the last admitted image that fails rather than the header parse.
#[test]
fn image_count_cap() {
    let limits = Limits::default();
    let count = limits.images_per_source + 1;
    let mut unit = Unit::new();
    for _ in 0..count {
        unit = unit.xml(
            r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="attachment:0:2"></Image>"#,
        );
    }
    let bytes = unit.build();

    let mut reader = Reader::seekable(Cursor::new(&bytes)).expect("construction succeeds");
    for i in 0..limits.images_per_source {
        assert!(
            reader.next_image().expect("advance within the cap"),
            "TestImageCountCap: advance {i} must succeed"
        );
    }
    let e = reader
        .next_image()
        .expect_err("TestImageCountCap: expected LimitExceeded and no value");
    assert_eq!(kind(&e), "LimitExceeded", "TestImageCountCap: {e}");
    assert_names("TestImageCountCap", &e, "images per source");

    // The other direction: the same file walks every image under a raised cap, which is what
    // shows the cap rather than the fixture is what stopped the walk.
    let mut raised = Limits::default();
    raised.images_per_source = count;
    let mut reader =
        Reader::seekable_with_limits(Cursor::new(&bytes), raised).expect("construction succeeds");
    let mut walked = 0u32;
    while reader.next_image().expect("advance under a raised cap") {
        walked += 1;
    }
    assert_eq!(walked, count, "TestImageCountCap: every image is walked");
}

// ==================================================================
// Cases the original corpus has no reason to cover, added here. The FITS-side malformed cases and
// the tile-compressed decline are already graded in `tests/fits_declines.rs` and are not
// duplicated.
// ==================================================================

/// Net-new: every original fixture that reaches the location check supplies one. §10 requires a
/// block's location and role to be completely defined by the header and §11.5 requires an
/// image's pixels to be a single data block, so an `<Image>` with no `location` at all is
/// `Malformed` — and it is reached only because the location check runs **last**, after every
/// attribute whose fault would otherwise be masked by it.
#[test]
fn no_location_at_all() {
    let e = expect_class(
        "no location at all",
        &raw_image(r#"geometry="2:2:1" sampleFormat="UInt16""#),
        "Malformed",
    );
    assert_names("no location at all", &e, "location");

    // And the ordering that makes the eighteen header-only ports classify as they do: an
    // unsupported attribute on the same element beats the missing location.
    expect_class(
        "no location at all (an earlier fault wins)",
        &raw_image(r#"geometry="2:2:1" sampleFormat="Complex32""#),
        "Unsupported",
    );
}

/// Net-new: a `Reference` whose `uid` names nothing. §11.13 permits forward references, so a
/// resolver cannot treat "not yet seen" as "absent" — and a dangling one is not an occurrence,
/// so the walk simply finds nothing rather than erroring or panicking.
#[test]
fn reference_to_a_nonexistent_uid() {
    let bytes = Unit::new()
        .xml(r#"<Reference ref="nothing-declares-this"/>"#)
        .build();
    let mut reader = Reader::seekable(Cursor::new(&bytes)).expect("construction succeeds");
    assert!(
        !reader.next_image().expect("advancing succeeds"),
        "a dangling Reference is not an image occurrence"
    );

    // The other direction, so the case cannot pass because references never resolve at all.
    let levels = repeating_u16(4);
    let resolving = Unit::new()
        .xml(r#"<Reference ref="the-image"/>"#)
        .attached(
            r#"<Image uid="the-image" geometry="2:2:1" sampleFormat="UInt16" {loc}/>"#,
            le_u16(&levels),
        )
        .build();
    let mut reader = Reader::seekable(Cursor::new(&resolving)).expect("construction succeeds");
    let mut walked = 0;
    while reader.next_image().expect("advance") {
        walked += 1;
    }
    assert_eq!(
        walked, 2,
        "a resolving Reference is an occurrence of its own"
    );
}

/// Net-new: §10.6 requires no validation of the subblock list and explicitly sets no upper
/// limit on its length, so all three checks here are this crate's. Without them the attribute
/// is a cheap amplification vector the element-count cap does not cover, the whole list being
/// one attribute string rather than elements.
#[test]
fn subblock_lengths_do_not_sum_to_the_block_size() {
    let levels = repeating_u16(8);
    let raw = le_u16(&levels);
    let block = zlib(&raw);
    let stored = block.len() as u64;

    // The compressed lengths must sum to the stored block size.
    let bytes = attach_image(
        &format!(
            r#"geometry="4:2:1" sampleFormat="UInt16" compression="zlib:16" subblocks="{},16""#,
            stored + 1
        ),
        block.clone(),
    );
    let e = expect_class(
        "subblock compressed lengths do not sum",
        &bytes,
        "Malformed",
    );
    assert_names("subblock compressed lengths do not sum", &e, "stored block");

    // And the uncompressed lengths must sum to the geometry-implied size.
    let bytes = attach_image(
        &format!(
            r#"geometry="4:2:1" sampleFormat="UInt16" compression="zlib:16" subblocks="{stored},15""#
        ),
        block.clone(),
    );
    let e = expect_class(
        "subblock uncompressed lengths do not sum",
        &bytes,
        "Malformed",
    );
    assert_names(
        "subblock uncompressed lengths do not sum",
        &e,
        "geometry implies",
    );

    // Both directions: the list that does sum decodes.
    let bytes = attach_image(
        &format!(
            r#"geometry="4:2:1" sampleFormat="UInt16" compression="zlib:16" subblocks="{stored},16""#
        ),
        block,
    );
    let image = decode(&bytes).expect("a summing subblock list decodes");
    assert_same_bits(
        "subblock list sums",
        image.samples(),
        &expected_u16(&levels),
    );
}

/// Net-new: § Deliberate divergences from prior art records that the prior decoder ignores `checksum` entirely,
/// so a file declaring a non-matching digest decodes clean there. That is the strongest
/// independent argument for verifying every checksum this crate reads.
#[test]
fn checksummed_block_whose_digest_does_not_match() {
    let levels = repeating_u16(4);
    let stored = le_u16(&levels);

    let wrong = format!(r#"checksum="sha-1:{}""#, "0".repeat(40));
    let bytes = attach_image(
        &format!(r#"geometry="2:2:1" sampleFormat="UInt16" {wrong}"#),
        stored.clone(),
    );
    let e = expect_class(
        "checksummed block whose digest does not match",
        &bytes,
        "ChecksumMismatch",
    );
    assert_names(
        "checksummed block whose digest does not match",
        &e,
        "digest",
    );

    // Both directions: the same block under its real digest decodes.
    let right = checksum_attr("sha-1", &stored);
    let bytes = attach_image(
        &format!(r#"geometry="2:2:1" sampleFormat="UInt16" {right}"#),
        stored,
    );
    let image = decode(&bytes).expect("a matching digest decodes");
    assert_same_bits("matching digest", image.samples(), &expected_u16(&levels));
}

/// Net-new: an XISF block behind the cursor on a sequential source is `Unsupported`, **not**
/// `Malformed` — the file is valid and self-consistent and the same bytes decode through
/// `Reader::seekable`. That distinction is the whole reason § Errors gives `Unsupported` its
/// "asks something of the *source* it cannot do" clause.
#[test]
fn a_block_behind_the_cursor_on_a_sequential_source() {
    let first = repeating_u16(4);
    let second = repeating_u16(8);
    let bytes = reversed_block_order(&le_u16(&first), &le_u16(&second));

    let mut reader = Reader::sequential(Cursor::new(&bytes[..])).expect("construction succeeds");
    assert!(reader.next_image().expect("advance"));
    // The first image's block is the *later* of the two, so decoding it carries the cursor past
    // the second image's block.
    let image = reader
        .read_image()
        .expect("the first image decodes forward");
    assert_same_bits(
        "sequential forward decode",
        image.samples(),
        &expected_u16(&first),
    );

    assert!(reader.next_image().expect("advance"));
    let e = match reader.read_image() {
        Ok(image) => panic!(
            "a block behind the cursor: expected Unsupported and no value; got {} samples",
            image.samples().len()
        ),
        Err(e) => e,
    };
    assert_eq!(kind(&e), "Unsupported", "a block behind the cursor: {e}");
    assert_names("a block behind the cursor", &e, "lies behind");

    // **Both directions, and this is the whole point of the class.** The identical bytes decode
    // through a seekable source: the file is valid and self-consistent, and what it asked was
    // something the *source* could not do. Classifying it `Malformed` would say the file was
    // broken, which these very bytes disprove.
    let mut reader = Reader::seekable(Cursor::new(&bytes[..])).expect("construction succeeds");
    assert!(reader.next_image().expect("advance"));
    let _ = reader.read_image().expect("the first image decodes");
    assert!(reader.next_image().expect("advance"));
    let image = reader
        .read_image()
        .expect("the second image decodes through a seekable source");
    assert_same_bits(
        "seekable backward decode",
        image.samples(),
        &expected_u16(&second),
    );
}

/// Two images whose blocks are stored in the **reverse** of their document order, which is what
/// puts a block behind a forward-only cursor. `Unit` lays attachments out in fragment order, so
/// this one is rendered here — with the convergence assertion § Testing mechanics requires,
/// the offsets' digit count feeding back into the header length that determines them.
fn reversed_block_order(first: &[u8], second: &[u8]) -> Vec<u8> {
    let mut base = 0u64;
    let mut header = String::new();
    let mut converged = false;
    for _ in 0..16 {
        header = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <xisf xmlns=\"http://www.pixinsight.com/xisf\" version=\"1.0\">\
             <Image geometry=\"2:2:1\" sampleFormat=\"UInt16\" location=\"attachment:{}:{}\"/>\
             <Image geometry=\"4:2:1\" sampleFormat=\"UInt16\" location=\"attachment:{}:{}\"/>\
             </xisf>",
            base + second.len() as u64,
            first.len(),
            base,
            second.len(),
        );
        let next = common::xisf::PREAMBLE as u64 + header.len() as u64;
        if next == base {
            converged = true;
            break;
        }
        base = next;
    }
    assert!(
        converged,
        "attachment offsets did not converge; widen the loop or pad the header"
    );

    let mut trailing = second.to_vec();
    trailing.extend_from_slice(first);
    raw_unit(
        b"XISF0100",
        u32::try_from(header.len()).expect("a fixture header fits u32"),
        &header,
        &trailing,
    )
}

/// **A row offset past `u64`, at the one site `check_total_samples` says it does not cover.**
///
/// `src/reader.rs`'s gate bounds *products of the geometry*, and its own comment says so —
/// then claimed the two sites that add a **file offset** on top of such a product were "written
/// checked in its own right". The XISF one was not: `next_chunk` computed
/// `position + input_row * stride` with a bare `+`, and `position` is not a product of the
/// geometry at all. It is parsed straight out of `location="attachment:P:S"`, so a **file**
/// states it and nothing bounds it on a source whose length the reader cannot see.
///
/// The reachability condition is a caller raising caps, which § The caps contemplates and which
/// the sibling case above is itself written around. Under the overflow checks every test and
/// fuzz build carries this was `attempt to add with overflow`; without them it wrapped to a
/// small offset, and the read then **succeeded against the wrong bytes**, which is worse.
///
/// The geometry is chosen so the byte extent still fits `u64` — otherwise the earlier gate
/// refuses it and this line is never reached, which is exactly why the two tests above pass
/// while this defect stood. What tips it over is `position`, and only `position`.
#[test]
fn a_row_offset_past_u64_is_refused_rather_than_computed() {
    // A source that reports a huge length and yields zeros, allocating nothing. A seek moves
    // no bytes, which is what makes a multi-gigabyte offset reachable in a unit test; the
    // forward-only spelling of the same shape needs the caller to raise `Skipped block bytes`
    // and then actually stream through it.
    struct Sparse {
        at: u64,
        head: Vec<u8>,
    }
    impl std::io::Read for Sparse {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len();
            for (i, slot) in buf.iter_mut().enumerate() {
                let at = self.at + i as u64;
                *slot = self.head.get(at as usize).copied().unwrap_or(0);
            }
            self.at += n as u64;
            Ok(n)
        }
    }
    impl std::io::Seek for Sparse {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.at = match pos {
                std::io::SeekFrom::Start(n) => n,
                // The whole address space. A file of unknown-but-enormous length is what
                // the `position + size > len` guard cannot refuse, and it is the condition
                // under which the row arithmetic below has to hold on its own.
                std::io::SeekFrom::End(n) => u64::MAX.saturating_add_signed(n),
                std::io::SeekFrom::Current(n) => self.at.saturating_add_signed(n),
            };
            Ok(self.at)
        }
    }

    // The byte extent fits `u64` with room, so the gate above passes and the walk reaches the
    // row arithmetic. Any wider and that gate refuses it first, which is precisely why the two
    // cases above pass while this defect stood.
    // One sample wide, so the row buffer this allocates is eight bytes and the test costs
    // nothing. The depth is in the other two axes, which is what puts the last channel's first
    // row 2^60 rows into the file.
    const W: u64 = 1;
    const H: u64 = 268_435_456; // 2^28
    const C: u64 = 4_294_967_295; // u32::MAX
    const BYTES: u64 = 8; // UInt64
    const EXTENT: u64 = W * H * C * BYTES; // just under 2^63

    // The declared attachment position, and the only thing that tips the sum over `u64`. A
    // file states this; it is not derived from the geometry, which is the entire point.
    const POSITION: u64 = u64::MAX - 4096;

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><xisf version="1.0" xmlns="http://www.pixinsight.com/xisf"><Image geometry="{W}:{H}:{C}" pixelStorage="Planar" sampleFormat="UInt64" location="attachment:{POSITION}:{EXTENT}"/></xisf>"#
    );
    let unit = common::xisf::raw_unit(b"XISF0100", xml.len() as u32, &xml, &[]);

    let mut raised = Limits::default();
    raised.total_samples = u64::MAX;
    raised.decoded_output_bytes = u64::MAX;
    raised.materialized_bytes = u64::MAX;
    raised.skipped_block_bytes = u64::MAX;
    raised.stored_block_multiple = u64::MAX;
    raised.stored_block_floor_bytes = u64::MAX;

    let source = Sparse { at: 0, head: unit };
    let mut reader = Reader::seekable_with_limits(source, raised).expect("the unit constructs");
    assert!(reader.next_image().expect("the position is reached"));
    // The last channel, so the first chunk already sits at `(C - 1) x H` rows in.
    reader
        .select_channel((C - 1) as u32)
        .expect("the last channel exists");
    let err = reader
        .chunks()
        .next_chunk()
        .expect_err("a row offset above u64 is refused rather than computed");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert_names("row offset above u64", &err, "row offset");
}
