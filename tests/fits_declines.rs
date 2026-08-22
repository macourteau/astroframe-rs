//! The FITS half of the decline table, graded row by row, plus the caller-misuse criteria.
//!
//! Every row asserts all three of its stated facts: the error **class**, the **surfacing
//! point** — construction, `next_image()`, or a declined position — and the **geometry** its
//! last column states. A declined position reporting `None` where the table says full
//! geometry, or the reverse, is a defect the class and the surfacing point both miss, so it
//! is asserted explicitly.
//!
//! Fixtures are built byte by byte through `tests/common`, and every class is derived from
//! § Errors' rules rather than from a case name.

// `tests/common/mod.rs` is shared, and its lints belong to that file's owner rather than to
// this suite, so they are silenced at the import instead of edited there.
mod common;

use std::io::Cursor;

use astroframe::{DeclineClass, Error, Limits, Reader, Samples, Sequential, Source};

use common::{FailingRead, Hdu, file};

// ------------------------------------------------------------------ helpers

type SeqReader = Reader<Sequential<Cursor<Vec<u8>>>>;

fn sequential(bytes: Vec<u8>) -> astroframe::Result<SeqReader> {
    Reader::sequential(Cursor::new(bytes))
}

fn sequential_with(bytes: Vec<u8>, limits: Limits) -> astroframe::Result<SeqReader> {
    Reader::sequential_with_limits(Cursor::new(bytes), limits)
}

/// The error's variant name. Assertions name the variant rather than matching on a message,
/// which is text the crate is free to reword.
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

/// Both pixel entry points at a declined position, since the table says *any* pixel call is
/// `Err` — grading only one would pass a decoder that checks the decline in just that one.
/// The zero-length destination is deliberate: the decline must be raised before the
/// destination is ever measured.
fn pixel_calls_fail<S: Source>(reader: &mut Reader<S>) -> Error {
    let chunked = reader
        .chunks()
        .next_chunk()
        .expect_err("chunked delivery declines");
    let whole = reader
        .read_image_into(&mut [])
        .expect_err("whole-image decode declines");
    assert_eq!(
        kind(&chunked),
        kind(&whole),
        "the two pixel entry points must decline alike: {chunked} / {whole}"
    );
    whole
}

/// The geometry three, as a triple, so a row's last column is one assertion.
fn geometry(header: &astroframe::Header) -> (Option<u32>, Option<u32>, Option<u32>) {
    (header.width(), header.height(), header.channels())
}

/// Compare buffers by `to_bits()`. `==` silently accepts a sign-of-zero difference, which is
/// exactly the difference this crate's normalization contract is about.
fn assert_same_bits(got: &[f32], want: &[f32], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: destination length");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(
            g.to_bits(),
            w.to_bits(),
            "{what}: sample {i}: got {g:?} ({:#010x}), want {w:?} ({:#010x})",
            g.to_bits(),
            w.to_bits()
        );
    }
}

/// The pinned normalization form, written out longhand so an expectation never comes from
/// the code under test: `physical = BSCALE * raw + BZERO`, then `(physical - lo) as f32`,
/// then a multiply by the `f32` reciprocal of the width.
fn expected_sample(raw: i16, bzero: f64, lo: f64, hi: f64) -> f32 {
    let physical: f64 = raw as f64 + bzero;
    let shifted: f32 = (physical - lo) as f32;
    shifted * (1.0f32 / ((hi - lo) as f32))
}

// ------------------------------------------------------------------ fixtures

/// A primary holding no image — the ordinary shape of every multi-extension file.
fn empty_primary() -> Hdu {
    Hdu::primary().card("BITPIX", "8").card("NAXIS", "0")
}

/// The stored samples of the 2x2 fixtures: the two endpoints of the unsigned convention and
/// two interior levels.
const STORED_2X2: [i16; 4] = [-32768, -32767, 0, 32767];

/// A decodable 2x2 frame under the FITS unsigned convention.
fn unsigned_2x2() -> Hdu {
    Hdu::primary()
        .image_2d(16, 2, 2)
        .unsigned_convention(16)
        .data_i16(&STORED_2X2)
}

/// The same frame as an `IMAGE` extension.
fn unsigned_2x2_extension() -> Hdu {
    Hdu::extension("IMAGE")
        .image_2d(16, 2, 2)
        .card("PCOUNT", "0")
        .card("GCOUNT", "1")
        .unsigned_convention(16)
        .data_i16(&STORED_2X2)
}

/// The 2x2 frame's expected normalized output at the default unsigned range.
fn unsigned_2x2_expected() -> Vec<f32> {
    STORED_2X2
        .iter()
        .map(|&raw| expected_sample(raw, 32768.0, 0.0, 65535.0))
        .collect()
}

/// A three-channel 2x2 cube, each plane holding distinct levels.
const STORED_2X2X3: [i16; 12] = [
    -32768, -32767, -32766, -32765, 0, 1, 2, 3, 32764, 32765, 32766, 32767,
];

fn unsigned_2x2x3() -> Hdu {
    Hdu::primary()
        .image_3d(16, 2, 2, 3)
        .unsigned_convention(16)
        .data_i16(&STORED_2X2X3)
}

/// A non-image extension carrying a small data unit.
fn table_extension() -> Hdu {
    Hdu::extension("TABLE")
        .card("BITPIX", "8")
        .card("NAXIS", "2")
        .card("NAXIS1", "4")
        .card("NAXIS2", "2")
        .card("PCOUNT", "0")
        .card("GCOUNT", "1")
        .data(vec![b'.'; 8])
}

/// An `IMAGE` extension declaring no data array. §7.1.1: no data blocks follow it.
fn null_image_extension() -> Hdu {
    Hdu::extension("IMAGE")
        .card("BITPIX", "8")
        .card("NAXIS", "0")
        .card("PCOUNT", "0")
        .card("GCOUNT", "1")
}

/// A tile-compressed image: a `BINTABLE` carrying `ZIMAGE = T`, whose geometry lives in the
/// `Z*` keywords while `NAXIS*` describe the table that holds the tiles.
fn tile_compressed_extension() -> Hdu {
    Hdu::extension("BINTABLE")
        .card("BITPIX", "8")
        .card("NAXIS", "2")
        .card("NAXIS1", "12")
        .card("NAXIS2", "2")
        .card("PCOUNT", "0")
        .card("GCOUNT", "1")
        .card("ZIMAGE", "T")
        .card("ZBITPIX", "16")
        .card("ZNAXIS", "2")
        .card("ZNAXIS1", "6")
        .card("ZNAXIS2", "4")
        .data(vec![0u8; 24])
}

// ------------------------------------------- rows that surface at construction

/// Criterion *Every decline surfaces where § Errors says it does*, row *Source matches
/// neither `SIMPLE` nor `XISF0100`*: `Malformed`, at construction, with no `Header`.
#[test]
fn source_matching_neither_signature_is_malformed_at_construction() {
    let err = sequential(b"NOTAFITS and not an XISF unit either".to_vec())
        .expect_err("a source matching neither signature");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(
        format!("{err}").contains("neither"),
        "the reason names what was expected: {err}"
    );
}

/// Row *`SIMPLE = F`*: `Unsupported`, at construction, with no `Header`.
#[test]
fn simple_f_is_unsupported_at_construction() {
    let bytes = file(&[Hdu::nonconforming_primary()
        .image_2d(16, 2, 2)
        .data_i16(&STORED_2X2)]);
    let err = sequential(bytes).expect_err("SIMPLE = F");
    // Unsupported, not Malformed: the file says plainly that it does not conform, which is a
    // valid statement this version declines rather than a contradiction.
    assert_eq!(kind(&err), "Unsupported", "{err}");
    assert!(format!("{err}").contains("SIMPLE = F"), "{err}");
}

/// Row *FITS header structure fault*, construction half: card grammar, no `END`, a truncated
/// header block, and a byte outside `0x20`–`0x7E` in a keyword-name or value field, all
/// `Malformed` at construction for the **primary** header.
#[test]
fn header_structure_fault_is_malformed_at_construction_for_the_primary() {
    let with_card = |bytes: &[u8]| {
        file(&[Hdu::primary()
            .image_2d(16, 2, 2)
            .raw(bytes)
            .data_i16(&STORED_2X2)])
    };
    let whole = file(&[unsigned_2x2()]);
    let cases: [(&str, Vec<u8>); 5] = [
        ("card grammar", with_card(b"OBJECT  = 'unterminated")),
        (
            "no END",
            Hdu::primary().image_2d(16, 2, 2).header_without_end(),
        ),
        ("truncated header block", whole[..1440].to_vec()),
        ("byte outside the set in a keyword name", {
            let mut name = b"OBJ".to_vec();
            name.push(0xff);
            name.extend_from_slice(b"CT  = 12");
            with_card(&name)
        }),
        ("byte outside the set in a value field", {
            let mut value = b"OBJECT  = 'M".to_vec();
            value.push(0xff);
            value.extend_from_slice(b"31'");
            with_card(&value)
        }),
    ];
    for (name, bytes) in cases {
        let err = sequential(bytes).expect_err(name);
        // A header that did not parse is not a declined position at all: there is nothing to
        // report about it and nothing to skip. A source that simply ends before `END` is
        // reached is the same fault seen from the other side, so both read `Malformed`.
        assert_eq!(kind(&err), "Malformed", "{name}: {err}");
    }
}

// ----------------------------------------- rows that surface at next_image()

/// Row *FITS header structure fault*, extension half: the same five faults in an
/// **extension's** header are `Malformed` from `next_image()`, which ends the walk.
#[test]
fn header_structure_fault_is_malformed_at_next_image_for_an_extension() {
    let with_card = |bytes: &[u8]| {
        file(&[
            empty_primary(),
            Hdu::extension("IMAGE")
                .image_2d(16, 2, 2)
                .card("PCOUNT", "0")
                .card("GCOUNT", "1")
                .raw(bytes),
        ])
    };
    let mut truncated = file(&[empty_primary()]);
    truncated.extend_from_slice(&file(&[unsigned_2x2_extension()])[..1440]);
    let mut without_end = file(&[empty_primary()]);
    without_end.extend_from_slice(
        &Hdu::extension("IMAGE")
            .image_2d(16, 2, 2)
            .header_without_end(),
    );

    let cases: [(&str, Vec<u8>); 5] = [
        ("card grammar", with_card(b"OBJECT  = 'unterminated")),
        ("no END", without_end),
        ("truncated header block", truncated),
        ("byte outside the set in a keyword name", {
            let mut name = b"OBJ".to_vec();
            name.push(0xff);
            name.extend_from_slice(b"CT  = 12");
            with_card(&name)
        }),
        ("byte outside the set in a value field", {
            let mut value = b"OBJECT  = 'M".to_vec();
            value.push(0xff);
            value.extend_from_slice(b"31'");
            with_card(&value)
        }),
    ];
    for (name, bytes) in cases {
        let mut reader = sequential(bytes).expect("the primary header parses");
        // `Err`, never the `Ok(false)` that means end of source.
        let err = reader.next_image().expect_err(name);
        assert_eq!(kind(&err), "Malformed", "{name}: {err}");
        assert!(
            reader.header().is_none(),
            "{name}: a header that did not parse is not a position, so nothing is reported"
        );
    }
}

/// Row *HDU-traversal cap tripped while advancing*: `LimitExceeded`, from `next_image()`,
/// with no `Header`.
#[test]
fn hdu_traversal_cap_is_limit_exceeded_at_next_image() {
    let bytes = file(&[
        empty_primary(),
        table_extension(),
        table_extension(),
        table_extension(),
        unsigned_2x2_extension(),
    ]);
    let mut limits = Limits::default();
    limits.hdus_per_advance = 2;
    let mut reader = sequential_with(bytes, limits).expect("the primary header parses");
    let err = reader.next_image().expect_err("the traversal cap trips");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("HDUs traversed"), "{err}");
    assert!(reader.header().is_none(), "no Header for a failed advance");
}

/// Row *A non-image extension whose data unit cannot be sized*: `Malformed`, from
/// `next_image()`, with no `Header`.
///
/// The walk ends there rather than guessing a resumption point, since inventing one
/// misparses every HDU after it.
#[test]
fn non_image_extension_that_cannot_be_sized_is_malformed_at_next_image() {
    let unsizable = Hdu::extension("TABLE")
        .card("BITPIX", "8")
        .card("NAXIS", "2")
        .card("NAXIS1", "4")
        .card("NAXIS2", "2")
        .card("GCOUNT", "1"); // PCOUNT is mandatory in an extension header and is absent
    let bytes = file(&[empty_primary(), unsizable, unsigned_2x2_extension()]);
    let mut reader = sequential(bytes).expect("the primary header parses");
    let err = reader
        .next_image()
        .expect_err("the data unit cannot be sized");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(format!("{err}").contains("PCOUNT"), "{err}");
    assert!(reader.header().is_none(), "no Header for a failed advance");
    // The walk is over: the image that follows is unreachable, because reaching it would
    // mean guessing where the unsizable data unit ended.
    assert!(
        !reader.next_image().expect("the walk has ended"),
        "no resumption point was invented"
    );
}

/// Row *A skipped data unit whose computed size exceeds the `Skipped block bytes` cap, on a
/// source that must read to skip*: `LimitExceeded`, from `next_image()`, with no `Header`.
#[test]
fn skipped_data_unit_past_the_cap_is_limit_exceeded_at_next_image() {
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
    limits.skipped_block_bytes = 2880;
    let mut reader = sequential_with(bytes, limits).expect("the primary header parses");
    let err = reader
        .next_image()
        .expect_err("the skipped-block cap trips");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("skipped block bytes"), "{err}");
    assert!(reader.header().is_none(), "no Header for a failed advance");
}

// --------------------------------------------------- declined-position rows

/// Row *`BITPIX` missing, unparseable, or outside {8, 16, 32, 64, −32, −64}*: `Malformed`, at
/// a declined position, reporting **full** geometry with `sample_format` `None`.
#[test]
fn bitpix_fault_is_malformed_at_a_declined_position_with_full_geometry() {
    let axes = |hdu: Hdu| {
        hdu.card("NAXIS", "2")
            .card("NAXIS1", "4")
            .card("NAXIS2", "3")
    };
    let cases = [
        ("missing", axes(Hdu::primary())),
        ("unparseable", axes(Hdu::primary().card("BITPIX", "abc"))),
        ("outside the set", axes(Hdu::primary().card("BITPIX", "5"))),
    ];
    for (name, hdu) in cases {
        let mut reader = sequential(file(&[hdu])).expect("the header parses");
        assert!(
            reader.next_image().expect("advancing succeeds"),
            "{name}: the reader sits on the position"
        );
        let header = reader.header().expect("a declined position still reports");
        assert_eq!(
            header.decline_reason().map(|d| d.class()),
            Some(DeclineClass::Malformed),
            "{name}"
        );
        assert_eq!(
            geometry(&header),
            (Some(4), Some(3), Some(1)),
            "{name}: the geometry reads even though BITPIX does not"
        );
        assert!(
            header.sample_format().is_none(),
            "{name}: a BITPIX outside the set has no representable sample width"
        );
        let err = pixel_calls_fail(&mut reader);
        assert_eq!(kind(&err), "Malformed", "{name}: {err}");
    }
}

/// Row *`NAXIS` or a `NAXISn` missing or unparseable*: `Malformed`, at a declined position,
/// reporting geometry `None`.
#[test]
fn naxis_fault_is_malformed_at_a_declined_position_with_no_geometry() {
    let cases = [
        ("NAXIS missing", Hdu::primary().card("BITPIX", "16")),
        (
            "NAXIS unparseable",
            Hdu::primary().card("BITPIX", "16").card("NAXIS", "two"),
        ),
        (
            "NAXIS1 missing",
            Hdu::primary()
                .card("BITPIX", "16")
                .card("NAXIS", "2")
                .card("NAXIS2", "4"),
        ),
        (
            "NAXIS2 unparseable",
            Hdu::primary()
                .card("BITPIX", "16")
                .card("NAXIS", "2")
                .card("NAXIS1", "4")
                .card("NAXIS2", "4.5"),
        ),
    ];
    for (name, hdu) in cases {
        let mut reader = sequential(file(&[hdu])).expect("the header parses");
        assert!(
            reader.next_image().expect("advancing succeeds"),
            "{name}: the reader sits on the position"
        );
        let header = reader.header().expect("a declined position still reports");
        assert_eq!(
            header.decline_reason().map(|d| d.class()),
            Some(DeclineClass::Malformed),
            "{name}"
        );
        assert_eq!(
            geometry(&header),
            (None, None, None),
            "{name}: the geometry three are reported as a unit, all None"
        );
        let err = pixel_calls_fail(&mut reader);
        assert_eq!(kind(&err), "Malformed", "{name}: {err}");
    }
}

/// Row *`PCOUNT` or `GCOUNT` missing or unparseable in an `IMAGE` extension header*:
/// `Malformed`, at a declined position, reporting **full** geometry.
#[test]
fn pcount_or_gcount_fault_in_an_image_extension_is_malformed_with_full_geometry() {
    let cases = [
        (
            "PCOUNT missing",
            Hdu::extension("IMAGE")
                .image_2d(16, 2, 3)
                .card("GCOUNT", "1"),
        ),
        (
            "GCOUNT unparseable",
            Hdu::extension("IMAGE")
                .image_2d(16, 2, 3)
                .card("PCOUNT", "0")
                .card("GCOUNT", "one"),
        ),
    ];
    for (name, ext) in cases {
        let mut reader =
            sequential(file(&[empty_primary(), ext])).expect("the primary header parses");
        assert!(
            reader.next_image().expect("advancing succeeds"),
            "{name}: the reader sits on the extension"
        );
        let header = reader.header().expect("a declined position still reports");
        assert_eq!(
            header.decline_reason().map(|d| d.class()),
            Some(DeclineClass::Malformed),
            "{name}"
        );
        assert_eq!(
            geometry(&header),
            (Some(2), Some(3), Some(1)),
            "{name}: the sizing keywords fault, the geometry still reads"
        );
        let err = pixel_calls_fail(&mut reader);
        assert_eq!(kind(&err), "Malformed", "{name}: {err}");
    }
}

/// Row *`NAXIS = 1`, `NAXIS > 3`*: `Unsupported`, at a declined position, reporting geometry
/// `None`.
#[test]
fn naxis_outside_two_or_three_is_unsupported_with_no_geometry() {
    let cases = [
        (
            "NAXIS = 1",
            Hdu::primary()
                .card("BITPIX", "16")
                .card("NAXIS", "1")
                .card("NAXIS1", "4"),
        ),
        (
            "NAXIS = 4",
            Hdu::primary()
                .card("BITPIX", "16")
                .card("NAXIS", "4")
                .card("NAXIS1", "2")
                .card("NAXIS2", "2")
                .card("NAXIS3", "2")
                .card("NAXIS4", "2"),
        ),
    ];
    for (name, hdu) in cases {
        let mut reader = sequential(file(&[hdu])).expect("the header parses");
        assert!(
            reader.next_image().expect("advancing succeeds"),
            "{name}: the reader sits on the position"
        );
        let header = reader.header().expect("a declined position still reports");
        // Unsupported, not Malformed: the header is valid FITS and self-consistent, and only
        // the scope this version reads excludes it.
        assert_eq!(
            header.decline_reason().map(|d| d.class()),
            Some(DeclineClass::Unsupported),
            "{name}"
        );
        assert_eq!(geometry(&header), (None, None, None), "{name}");
        let err = pixel_calls_fail(&mut reader);
        assert_eq!(kind(&err), "Unsupported", "{name}: {err}");
    }
}

/// Row *`NAXISn = 0` (legal FITS, no data)*: `Unsupported`, at a declined position, reporting
/// **full** geometry.
///
/// Deliberately distinct from `NAXIS = 0`, which is not an image position at all: a
/// zero-valued `NAXISn` declares an image with a degenerate axis.
#[test]
fn naxisn_zero_is_unsupported_with_full_geometry() {
    let mut reader =
        sequential(file(&[Hdu::primary().image_2d(16, 0, 4)])).expect("the header parses");
    assert!(
        reader.next_image().expect("advancing succeeds"),
        "a degenerate axis is still an image position"
    );
    let header = reader.header().expect("a declined position still reports");
    assert_eq!(
        header.decline_reason().map(|d| d.class()),
        Some(DeclineClass::Unsupported)
    );
    assert_eq!(
        geometry(&header),
        (Some(0), Some(4), Some(1)),
        "a zero-length axis is representable, so it is reported"
    );
    let err = pixel_calls_fail(&mut reader);
    assert_eq!(kind(&err), "Unsupported", "{err}");
}

/// Row *`GROUPS = T` (random groups, with `NAXIS1 = 0`)*: `Unsupported`, at a declined
/// position, reporting **full** geometry.
///
/// The fixture carries `NAXIS1 = 0` as the row states, so it would trip the `NAXISn = 0` row
/// too: the validation order puts `GROUPS` first, which is what makes the class determinate.
#[test]
fn groups_t_is_unsupported_with_full_geometry() {
    let hdu = Hdu::primary()
        .card("BITPIX", "8")
        .card("NAXIS", "3")
        .card("NAXIS1", "0")
        .card("NAXIS2", "4")
        .card("NAXIS3", "2")
        .card("GROUPS", "T");
    let mut reader = sequential(file(&[hdu])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let header = reader.header().expect("a declined position still reports");
    assert_eq!(
        header.decline_reason().map(|d| d.class()),
        Some(DeclineClass::Unsupported)
    );
    assert!(
        header
            .decline_reason()
            .is_some_and(|d| d.reason().contains("GROUPS")),
        "GROUPS is settled before the degenerate-axis scope check: {:?}",
        header.decline_reason()
    );
    assert_eq!(geometry(&header), (Some(0), Some(4), Some(2)));
    let err = pixel_calls_fail(&mut reader);
    assert_eq!(kind(&err), "Unsupported", "{err}");
}

/// Row *`BINTABLE` with `ZIMAGE = T` (tile-compressed)*: `Unsupported`, at a declined
/// position, reporting **full** geometry from the `Z*` keywords.
#[test]
fn bintable_with_zimage_is_unsupported_with_geometry_from_the_z_keywords() {
    let bytes = file(&[empty_primary(), tile_compressed_extension()]);
    let mut reader = sequential(bytes).expect("the primary header parses");
    assert!(
        reader.next_image().expect("advancing succeeds"),
        "a tile-compressed BINTABLE is a declined position, not an extension to step over"
    );
    let header = reader.header().expect("a declined position still reports");
    assert_eq!(
        header.decline_reason().map(|d| d.class()),
        Some(DeclineClass::Unsupported)
    );
    // The `Z*` keywords, not the `NAXIS*` ones that describe the table holding the tiles.
    assert_eq!(geometry(&header), (Some(6), Some(4), Some(1)));
    assert_eq!(header.granularity(), astroframe::Granularity::WholeImage);
    let err = pixel_calls_fail(&mut reader);
    assert_eq!(kind(&err), "Unsupported", "{err}");
}

// --------------------------------------------------------- the *Neither* rows

/// Row *Primary `NAXIS = 0`, no image extension found*: **Neither** — the walk ends normally
/// with no error, and `header()` is `None`.
#[test]
fn primary_naxis_zero_with_no_image_extension_ends_the_walk_normally() {
    for (name, bytes) in [
        ("nothing after it", file(&[empty_primary()])),
        (
            "only non-image extensions after it",
            file(&[empty_primary(), table_extension(), table_extension()]),
        ),
    ] {
        let mut reader = sequential(bytes).expect("the primary header parses");
        assert!(
            !reader
                .next_image()
                .expect("a source with no image is not an error"),
            "{name}: end of source is Ok(false)"
        );
        assert!(reader.header().is_none(), "{name}");
    }
}

/// Row *`XTENSION = 'IMAGE'` with `NAXIS = 0`*: **Neither** — not an image position, so it is
/// skipped inside `next_image()` exactly as a non-image extension is.
#[test]
fn image_extension_with_naxis_zero_is_stepped_over() {
    // Stepped over on the way to a real image.
    let bytes = file(&[
        empty_primary(),
        null_image_extension(),
        unsigned_2x2_extension(),
    ]);
    let mut reader = sequential(bytes).expect("the primary header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let header = reader.header().expect("an image position reports");
    assert!(
        header.decline_reason().is_none(),
        "the image itself decodes"
    );
    assert_eq!(geometry(&header), (Some(2), Some(2), Some(1)));
    let mut dst = [0.0f32; 4];
    reader.read_image_into(&mut dst).expect("the image decodes");
    assert_same_bits(&dst, &unsigned_2x2_expected(), "past a NAXIS = 0 extension");

    // §7.1.1: such an extension has no data blocks following its header, so stepping over it
    // costs no `Skipped block bytes` at all — a cap of zero still walks past it.
    let bytes = file(&[
        empty_primary(),
        null_image_extension(),
        unsigned_2x2_extension(),
    ]);
    let mut limits = Limits::default();
    limits.skipped_block_bytes = 0;
    let mut reader = sequential_with(bytes, limits).expect("the primary header parses");
    assert!(
        reader
            .next_image()
            .expect("nothing is read to step over it"),
        "an exact skip of zero bytes consults no read cap"
    );

    // And a file made only of such extensions holds no image, which is end of source.
    let bytes = file(&[empty_primary(), null_image_extension()]);
    let mut reader = sequential(bytes).expect("the primary header parses");
    assert!(
        !reader
            .next_image()
            .expect("a source with no image is not an error"),
        "a file of NAXIS = 0 extensions walks zero images"
    );
    assert!(reader.header().is_none());
}

/// Rows *`BITPIX` missing…* and *`PCOUNT` or `GCOUNT` missing…*, second half: the **next**
/// `next_image()` from a declined position whose data unit cannot be sized returns
/// `Err(Malformed)` rather than `Ok(false)` or a guessed skip.
///
/// The one rule surfaces in two places, and the only difference is whether the caller sees
/// the position first. Here it does: the reader sat on it and reported it, and the walk ends
/// on the advance past it, because inventing a resumption point misparses every HDU after it.
#[test]
fn a_declined_position_that_cannot_be_sized_ends_the_walk_on_the_next_advance() {
    // BITPIX is what both the decline and the size formula need, so its absence declines the
    // position and then makes it unskippable.
    let declined = Hdu::primary()
        .card("NAXIS", "2")
        .card("NAXIS1", "4")
        .card("NAXIS2", "3");
    let bytes = file(&[declined, unsigned_2x2_extension()]);
    let mut reader = sequential(bytes).expect("the header parses");
    assert!(
        reader.next_image().expect("advancing succeeds"),
        "the reader sits on the declined position"
    );
    assert!(
        reader
            .header()
            .and_then(|h| h.decline_reason().map(|d| d.class()))
            .is_some(),
        "the position is reported before the walk ends"
    );
    let err = reader
        .next_image()
        .expect_err("there is no resumption point past an unsizable data unit");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(format!("{err}").contains("resumption point"), "{err}");
}

/// **The same rule where the size is computable and the *file offset past it* is not.**
///
/// § Errors names "size arithmetic that overflows the `u64` the size computation runs in" as a
/// `Malformed` outcome from the next advance. The sibling above reaches it by making the size
/// uncomputable; this reaches it by making the size legal and enormous.
///
/// `data_unit_size` is checked throughout and returns `18446744073709549440` here — the block
/// padding of `9223372036854774368` 16-bit samples, and the largest such multiple below
/// `u64::MAX`. Adding the 2880-byte data start to that overflows, which was an
/// `attempt to add with overflow` panic under the overflow checks the test and fuzz profiles
/// use, and a silent wrap to `Ok(false)` without them. **Invariant I5, from a 2880-byte file
/// with no cap raised.**
///
/// The fuzzers reach this code and cannot find it: the trigger is one specific 19-digit
/// decimal constant, which is the blind spot `tests/COVERAGE.md` records for exactly this
/// class. A value close enough to matter is not close enough to reach — only the arithmetic
/// tells you which values overflow, so this is a test written from the arithmetic.
#[test]
fn a_data_unit_whose_extent_ends_past_u64_is_malformed_on_the_next_advance() {
    // NAXIS = 1 declines the position (this version reads NAXIS 2, or 3 as channels), so the
    // reader sits on it and reports it, and the overflow happens on the advance past it.
    let declined = Hdu::primary()
        .card("BITPIX", "16")
        .card("NAXIS", "1")
        .card("NAXIS1", "9223372036854774368");
    let bytes = file(&[declined, unsigned_2x2_extension()]);
    assert_eq!(bytes.len() % 2880, 0, "the fixture is whole blocks");
    let mut reader = sequential(bytes).expect("the header parses");
    assert!(
        reader.next_image().expect("advancing succeeds"),
        "the reader sits on the declined position"
    );
    let err = reader
        .next_image()
        .expect_err("a data unit ending past u64 has no resumption point");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(format!("{err}").contains("resumption point"), "{err}");
    assert!(format!("{err}").contains("past u64"), "{err}");
}

// ------------------------------------------------------ the validation order

/// Criterion *Every decline surfaces where § Errors says it does* — the validation order
/// itself, which is what makes the classes determinate.
///
/// A header carrying both `BITPIX = 5` and `NAXIS = 1` is `Malformed`, not `Unsupported`:
/// structural validity of a value is settled before scope is.
#[test]
fn structural_validity_is_settled_before_scope() {
    let hdu = Hdu::primary()
        .card("BITPIX", "5")
        .card("NAXIS", "1")
        .card("NAXIS1", "4");
    let mut reader = sequential(file(&[hdu])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let header = reader.header().expect("a declined position still reports");
    let decline = header.decline_reason().expect("the position is declined");
    assert_eq!(decline.class(), DeclineClass::Malformed, "{decline:?}");
    assert!(
        decline.reason().contains("BITPIX"),
        "the BITPIX fault wins over the NAXIS scope decline: {decline:?}"
    );
    let err = pixel_calls_fail(&mut reader);
    assert_eq!(kind(&err), "Malformed", "{err}");
}

// ------------------------------------- declining is catchable, and distinguishable

/// Criterion *Declining is catchable, and distinguishable from failing*: a truncated file is
/// `Malformed`, and explicitly **not** `Io` — a short file is bad data, not a failing disk.
#[test]
fn a_truncated_file_is_malformed_and_not_io() {
    let whole = file(&[unsigned_2x2()]);
    // The whole header plus one sample of the data unit.
    let mut reader = sequential(whole[..2880 + 2].to_vec()).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 4];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("the data unit is short");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(!err.is_io(), "truncation is not an I/O failure: {err}");
    assert!(format!("{err}").contains("truncated"), "{err}");
}

/// Criterion *Declining is catchable, and distinguishable from failing*: a source whose reads
/// fail mid-read is `Io`, with `is_io()` true — the one class whose documented move is abort.
#[test]
fn a_source_whose_reads_fail_is_io() {
    let bytes = file(&[unsigned_2x2()]);

    // Failing inside the header region, so the failure surfaces at construction.
    let Err(err) = Reader::sequential(FailingRead {
        data: bytes.clone(),
        pos: 0,
        ok_bytes: 100,
    }) else {
        panic!("a source that fails mid-header must not construct a Reader");
    };
    assert_eq!(kind(&err), "Io", "{err}");
    assert!(err.is_io(), "the skip-versus-abort split: {err}");

    // Failing inside the data unit, so the header parses and the failure surfaces at the
    // pixel call.
    let mut reader = Reader::sequential(FailingRead {
        data: bytes,
        pos: 0,
        ok_bytes: 2880 + 2,
    })
    .expect("the header region reads");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 4];
    let err = reader
        .read_image_into(&mut dst)
        .expect_err("the source fails mid-data-unit");
    assert_eq!(kind(&err), "Io", "{err}");
    assert!(err.is_io(), "{err}");
}

/// Criterion *Declining is catchable, and distinguishable from failing*: a tile-compressed
/// file yields `Unsupported` rather than being misread as a table.
///
/// The real shape of such a file: a `NAXIS = 0` primary, no image extension, one `BINTABLE`
/// carrying `ZIMAGE = T`. Misreading it as a table would walk straight past the only image in
/// the file and report end of source.
#[test]
fn a_tile_compressed_file_is_unsupported_rather_than_misread_as_a_table() {
    let bytes = file(&[empty_primary(), tile_compressed_extension()]);
    let mut reader = sequential(bytes).expect("the primary header parses");
    assert!(
        reader.next_image().expect("advancing succeeds"),
        "the tile-compressed BINTABLE is found rather than stepped over"
    );
    let err = pixel_calls_fail(&mut reader);
    assert_eq!(kind(&err), "Unsupported", "{err}");
    assert!(
        format!("{err}").contains("tile-compressed"),
        "the reason names the feature declined: {err}"
    );
}

// ------------------------------------------------ non-conforming header text

/// Criterion *Non-conforming header text is handled by the stated rule*: non-ASCII bytes in a
/// `COMMENT` card are tolerated — the frame parses and its geometry survives.
#[test]
fn non_ascii_in_a_comment_card_parses_and_geometry_survives() {
    let hdu = Hdu::primary()
        .image_2d(16, 2, 2)
        .unsigned_convention(16)
        .commentary("COMMENT", "seeing was naïve — ★ 2.1″")
        .commentary("HISTORY", "stacked ×16")
        .data_i16(&STORED_2X2);
    let mut reader = sequential(file(&[hdu])).expect("a tolerated byte does not fail the frame");
    assert!(reader.next_image().expect("advancing succeeds"));
    let header = reader.header().expect("an image position reports");
    assert!(header.decline_reason().is_none(), "the frame is decodable");
    assert_eq!(geometry(&header), (Some(2), Some(2), Some(1)));
    // The text is decoded lossily rather than dropped, so the card is still reported.
    assert!(
        header
            .get("COMMENT")
            .is_some_and(|k| !k.comment().unwrap_or_default().is_empty()),
        "the commentary text is reported"
    );
    let mut dst = [0.0f32; 4];
    reader.read_image_into(&mut dst).expect("the frame decodes");
    assert_same_bits(&dst, &unsigned_2x2_expected(), "non-ASCII commentary");
}

/// Criterion *Non-conforming header text is handled by the stated rule*: the same bytes in a
/// **value field** are a hard error — a value that cannot be read as the standard defines it
/// cannot be trusted downstream.
#[test]
fn non_ascii_in_a_value_field_is_a_hard_error() {
    let high = {
        let mut card = b"OBJECT  = 'M".to_vec();
        card.push(0xc3);
        card.push(0xa9);
        card.extend_from_slice(b"31'");
        card
    };
    // The rule is a *range*, not "non-ASCII": a control byte below 0x20 is outside it too, and
    // it is the half an implementation checking `is_ascii()` lets through.
    let control = {
        let mut card = b"EXPTIME = 12".to_vec();
        card.push(0x09);
        card.extend_from_slice(b"34");
        card
    };
    for (name, card) in [("above the set", high), ("below the set", control)] {
        let hdu = Hdu::primary()
            .image_2d(16, 2, 2)
            .raw(&card)
            .data_i16(&STORED_2X2);
        let err = sequential(file(&[hdu])).expect_err("a value field is not tolerated");
        assert_eq!(kind(&err), "Malformed", "{name}: {err}");
        assert!(format!("{err}").contains("character set"), "{name}: {err}");
    }
}

/// Criterion *Non-conforming header text is handled by the stated rule*: the same bytes in a
/// **keyword-name field** are a hard error — a name that cannot be represented cannot be
/// matched, and `get` promises exact matching.
#[test]
fn non_ascii_in_a_keyword_name_field_is_a_hard_error() {
    let mut card = b"OBJ".to_vec();
    card.push(0xc3);
    card.extend_from_slice(b"CT  = 12");
    let hdu = Hdu::primary()
        .image_2d(16, 2, 2)
        .raw(&card)
        .data_i16(&STORED_2X2);
    let err = sequential(file(&[hdu])).expect_err("a keyword name is not a tolerated position");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(format!("{err}").contains("character set"), "{err}");
}

// -------------------------------------- caller misuse is an error, not a panic

/// Criterion *Caller misuse is an error, not a panic*: a wrong-sized destination is
/// `InvalidRequest`, both **too small and too large** — the length is checked with `==`, not
/// `>=`, so an oversized slice is as much a caller error as an undersized one.
#[test]
fn a_wrong_sized_destination_is_invalid_request() {
    let bytes = file(&[unsigned_2x2()]);

    for (name, len) in [("too small", 3usize), ("too large", 5)] {
        let mut reader = sequential(bytes.clone()).expect("the header parses");
        assert!(reader.next_image().expect("advancing succeeds"));
        let mut dst = vec![0.0f32; len];
        let err = reader
            .read_image_into(&mut dst)
            .expect_err("a wrong-sized destination");
        assert_eq!(kind(&err), "InvalidRequest", "{name}: {err}");

        let mut reader = sequential(bytes.clone()).expect("the header parses");
        assert!(reader.next_image().expect("advancing succeeds"));
        let mut native = Samples::I16(vec![0; len]);
        let err = reader
            .read_samples_into(&mut native)
            .expect_err("a wrong-sized native destination");
        assert_eq!(kind(&err), "InvalidRequest", "{name}: {err}");
    }
}

/// Criterion *Caller misuse is an error, not a panic*: `select_channel` beyond the channel
/// count is `InvalidRequest`.
#[test]
fn select_channel_beyond_the_channel_count_is_invalid_request() {
    let mut reader = sequential(file(&[unsigned_2x2x3()])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    reader.select_channel(2).expect("the last channel exists");
    let err = reader.select_channel(3).expect_err("one past the last");
    assert_eq!(kind(&err), "InvalidRequest", "{err}");
    assert!(format!("{err}").contains("3 channels"), "{err}");
}

/// Criterion *Caller misuse is an error, not a panic*: `select_channel` at a position whose
/// channel count is `None` is `InvalidRequest` for every `k`, there being no channel count for
/// `k` to be within.
#[test]
fn select_channel_where_the_channel_count_is_none_is_invalid_request() {
    let hdu = Hdu::primary()
        .card("BITPIX", "16")
        .card("NAXIS", "1")
        .card("NAXIS1", "4");
    let mut reader = sequential(file(&[hdu])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    assert!(
        reader.header().and_then(|h| h.channels()).is_none(),
        "the fixture is a position reporting no geometry"
    );
    let err = reader.select_channel(0).expect_err("no channel count");
    assert_eq!(kind(&err), "InvalidRequest", "{err}");
}

/// Criterion *Caller misuse is an error, not a panic*: `with_bounds` after the pixel phase has
/// begun is `InvalidRequest`.
#[test]
fn with_bounds_after_the_pixel_phase_is_invalid_request() {
    let mut reader = sequential(file(&[unsigned_2x2()])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut dst = [0.0f32; 4];
    reader.read_image_into(&mut dst).expect("the frame decodes");
    let err = reader
        .with_bounds(0.0, 4095.0)
        .expect_err("configuration is fixed from the pixel phase on");
    assert_eq!(kind(&err), "InvalidRequest", "{err}");

    // The same holds for the narrowing call, and for a cursor taken but never advanced:
    // holding a chunk cursor has already committed the reader.
    let mut reader = sequential(file(&[unsigned_2x2x3()])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let _ = reader.chunks();
    let err = reader
        .select_channel(1)
        .expect_err("the pixel phase has begun");
    assert_eq!(kind(&err), "InvalidRequest", "{err}");
}

/// Criterion *Caller misuse is an error, not a panic*: `read_samples_into` with a `Samples`
/// variant that does not match the header's sample format is `InvalidRequest`, not a silent
/// conversion.
#[test]
fn read_samples_into_with_a_mismatched_variant_is_invalid_request() {
    let bytes = file(&[unsigned_2x2()]);
    let mut reader = sequential(bytes.clone()).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    // BITPIX = 16 stores *signed* 16-bit samples; the unsigned convention is a scaling, not a
    // storage type, so a U16 destination is the plausible wrong request.
    let mut wrong = Samples::U16(vec![0; 4]);
    let err = reader
        .read_samples_into(&mut wrong)
        .expect_err("a mismatched sample variant");
    assert_eq!(kind(&err), "InvalidRequest", "{err}");

    let mut reader = sequential(bytes).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    let mut right = Samples::I16(vec![0; 4]);
    reader
        .read_samples_into(&mut right)
        .expect("the matching variant decodes");
    assert_eq!(right, Samples::I16(STORED_2X2.to_vec()));
}

/// Criterion *Caller misuse is an error, not a panic*, graded the other way: a second
/// `with_bounds` and a second `select_channel` *before* the pixel phase each succeed, and the
/// frame decodes against the **later** value.
#[test]
fn a_second_with_bounds_and_a_second_select_channel_are_last_wins() {
    let mut reader = sequential(file(&[unsigned_2x2()])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    reader.with_bounds(0.0, 4095.0).expect("the first range");
    reader
        .with_bounds(0.0, 131_070.0)
        .expect("a second call is not an error");
    let mut dst = [0.0f32; 4];
    reader.read_image_into(&mut dst).expect("the frame decodes");
    let want: Vec<f32> = STORED_2X2
        .iter()
        .map(|&raw| expected_sample(raw, 32768.0, 0.0, 131_070.0))
        .collect();
    assert_same_bits(&dst, &want, "the last with_bounds wins");

    let mut reader = sequential(file(&[unsigned_2x2x3()])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    reader.select_channel(2).expect("the first narrowing");
    reader
        .select_channel(0)
        .expect("narrowing runs from the file's channels each time");
    assert_eq!(
        reader.header().and_then(|h| h.channels()),
        Some(1),
        "a narrowed reader reports one channel"
    );
    let mut dst = [0.0f32; 4];
    reader.read_image_into(&mut dst).expect("the frame decodes");
    let want: Vec<f32> = STORED_2X2X3[..4]
        .iter()
        .map(|&raw| expected_sample(raw, 32768.0, 0.0, 65535.0))
        .collect();
    assert_same_bits(&dst, &want, "the last select_channel wins");
}

/// Criterion *Caller misuse is an error, not a panic*: a second `with_bounds` carrying an
/// **invalid** range is `InvalidRequest` and **leaves the first range in force** — each call is
/// validated on its own, so a rejected one clears nothing.
#[test]
fn an_invalid_second_with_bounds_leaves_the_first_range_in_force() {
    let mut reader = sequential(file(&[unsigned_2x2()])).expect("the header parses");
    assert!(reader.next_image().expect("advancing succeeds"));
    reader.with_bounds(0.0, 131_070.0).expect("the first range");
    // `lo == hi` fails the one range-validity rule: the reciprocal of the width is not finite.
    let err = reader
        .with_bounds(7.0, 7.0)
        .expect_err("an invalid second range");
    assert_eq!(kind(&err), "InvalidRequest", "{err}");

    let mut dst = [0.0f32; 4];
    reader.read_image_into(&mut dst).expect("the frame decodes");
    let want: Vec<f32> = STORED_2X2
        .iter()
        .map(|&raw| expected_sample(raw, 32768.0, 0.0, 131_070.0))
        .collect();
    // The pixels are the assertion: an implementation that cleared the range on the rejected
    // call would still return `InvalidRequest` and then decode against the format default.
    assert_same_bits(
        &dst,
        &want,
        "the rejected call left the first range in force",
    );
}
