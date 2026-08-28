//! The XISF half of the decline table, graded row by row, plus the XISF validation order and
//! the multi-image half of *Multi-image and source-mode behaviour*.
//!
//! Every row asserts all three of its stated facts: the error **class**, the **surfacing
//! point** — construction, `next_image()`, or a declined position — and the **geometry** its
//! last column states. A declined position reporting `None` where the table says full
//! geometry, or the reverse, is a defect the class and the surfacing point both miss, so it is
//! asserted explicitly.
//!
//! A **declined position** means all four of: construction succeeds, `next_image()` returns
//! `Ok(true)`, `header()` reports what it can with `decline_reason()` set, and any pixel call
//! returns `Err`. [`declined`] asserts all four at once so no row can grade three of them.
//!
//! Fixtures are built byte by byte through `tests/common/xisf.rs`, and every class is derived
//! from § Errors' rules rather than from a case name.

#![forbid(unsafe_code)]

mod common;

use std::io::Cursor;

use astroframe::{DeclineClass, Error, Header, Limits, Reader, Seekable, Sequential, Source};
use common::xisf::{
    Unit, base64, checksum_attr, expected_u16, le_u16, raw_unit, repeating_u16, samples,
    with_header,
};
use common::{assert_same_bits, kind};

// ------------------------------------------------------------------ helpers

type SeekReader = Reader<Seekable<Cursor<Vec<u8>>>>;

fn seekable(bytes: Vec<u8>) -> astroframe::Result<SeekReader> {
    Reader::seekable(Cursor::new(bytes))
}

fn sequential(bytes: Vec<u8>) -> astroframe::Result<Reader<Sequential<Cursor<Vec<u8>>>>> {
    Reader::sequential(Cursor::new(bytes))
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
type Geometry = (Option<u32>, Option<u32>, Option<u32>);

fn geometry(header: &Header) -> Geometry {
    (header.width(), header.height(), header.channels())
}

/// All four facts a **declined position** row asserts, in one call.
///
/// Returns the header — so the row can grade its geometry column — and the class the pixel
/// calls actually raised, which must match the class the header reported.
fn declined(what: &str, bytes: Vec<u8>) -> (Header, DeclineClass) {
    let mut reader = seekable(bytes).unwrap_or_else(|e| panic!("{what}: the unit constructs: {e}"));
    assert!(
        reader
            .next_image()
            .unwrap_or_else(|e| panic!("{what}: the walk advances: {e}")),
        "{what}: a declined position is still a position"
    );
    let header = reader
        .header()
        .unwrap_or_else(|| panic!("{what}: a declined position still reports"));
    let decline = header
        .decline_reason()
        .unwrap_or_else(|| panic!("{what}: decline_reason is Some"))
        .clone();
    let err = pixel_calls_fail(&mut reader);
    // The class it reported is the class it raises; a position that reports one and raises
    // another is unusable for the batch consumer the accessor exists for.
    assert_eq!(
        kind(&err),
        format!("{:?}", decline.class()),
        "{what}: {err}"
    );
    (header, decline.class())
}

/// A one-image unit whose `<Image>` is written attribute by attribute and whose block is
/// attached, for the rows whose point is one faulty attribute.
fn attached(attrs: &str) -> Vec<u8> {
    Unit::new()
        .attached(
            &format!("<Image {attrs} {{loc}}/>"),
            le_u16(&repeating_u16(12)),
        )
        .build()
}

/// A one-image unit whose `<Image>` carries **no** `location` at all, and so no attachment.
///
/// This is the shape the validation order is really about: several fixtures carry an
/// unsupported *attribute* and no `location`, and they yield `Unsupported` only because the
/// location check runs last.
fn unattached(attrs: &str) -> Vec<u8> {
    Unit::new().xml(&format!("<Image {attrs}/>")).build()
}

/// A one-image unit whose `<Image>` carries children and an otherwise faultless attribute set,
/// for the rows whose point is one faulty ancillary element.
fn with_children(children: &str) -> Vec<u8> {
    Unit::new()
        .attached(
            &format!(r#"<Image geometry="4:3:1" sampleFormat="UInt16" {{loc}}>{children}</Image>"#),
            le_u16(&repeating_u16(12)),
        )
        .build()
}

// ------------------------------------------------------- rows that surface at construction

/// Row *Source matches neither `SIMPLE` nor `XISF0100`*: `Malformed`, at construction, with no
/// `Header`.
#[test]
fn a_source_matching_neither_signature_is_malformed_at_construction() {
    let err = seekable(b"NOTAFILE\x00\x01\x02\x03and some more bytes".to_vec())
        .expect_err("neither signature");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(format!("{err}").contains("XISF0100"), "{err}");

    // A source too short to identify at all lands in the same class: a short file is bad data,
    // not a failing disk, so it is never `Io`.
    let err = seekable(b"XISF".to_vec()).expect_err("four bytes");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// Row *XISF signature of another version (`XISF0200`), or root `version` other than `1.0`*:
/// `Unsupported`, at construction, with no `Header`.
///
/// `Unsupported` rather than `Malformed` in both halves: the file is valid and self-consistent
/// and says plainly which version it is, and a later version may redefine what this crate
/// reads.
#[test]
fn another_xisf_version_is_unsupported_at_construction_from_either_declaration() {
    // The signature half. A well-formed 1.0 header behind a 2.0 signature, so the only thing
    // refused is the version.
    let mut whole = Unit::new().image_u16(4, 3, 1, &samples()).build();
    whole[..8].copy_from_slice(b"XISF0200");
    let err = seekable(whole).expect_err("the XISF0200 signature");
    assert_eq!(kind(&err), "Unsupported", "{err}");
    assert!(format!("{err}").contains("XISF0200"), "{err}");

    // The root-attribute half, which §9.5 makes mandatory and which the signature does not
    // duplicate.
    let bytes = Unit::new()
        .root_attrs(r#" xmlns="http://www.pixinsight.com/xisf" version="1.1""#)
        .image_u16(4, 3, 1, &samples())
        .build();
    let err = seekable(bytes).expect_err("root version 1.1");
    assert_eq!(kind(&err), "Unsupported", "{err}");
    assert!(format!("{err}").contains("1.1"), "{err}");

    // A missing `version` is the file contradicting §9.5 rather than declaring a version this
    // crate declines, so it is the other class.
    let bytes = Unit::new()
        .root_attrs(r#" xmlns="http://www.pixinsight.com/xisf""#)
        .image_u16(4, 3, 1, &samples())
        .build();
    let err = seekable(bytes).expect_err("no root version");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// Row *XISF unit-level fault: unparseable or oversized XML header, wrong root element,
/// tripped XML guard*: `Malformed` at construction — `LimitExceeded` for a guard — with no
/// `Header`.
#[test]
fn a_unit_level_fault_fails_at_construction_rather_than_declining_a_position() {
    let padding = " ".repeat(64);

    // Unparseable: a start tag that never closes.
    let err = seekable(with_header(
        &format!(r#"<xisf version="1.0"><Image geometry="4:3:1"{padding}"#),
        &[],
    ))
    .expect_err("unparseable XML");
    assert_eq!(kind(&err), "Malformed", "{err}");

    // Oversized: the preamble declares more header than the source holds. Truncation is
    // `Malformed`, not `Io` — a short file is bad data.
    let short = format!(r#"<xisf version="1.0"></xisf>{padding}"#);
    let err = seekable(raw_unit(b"XISF0100", 100_000, &short, &[])).expect_err("a short source");
    assert_eq!(kind(&err), "Malformed", "{err}");

    // Wrong root element: a well-formed document that is not an XISF unit.
    let err = seekable(with_header(
        &format!(r#"<notxisf version="1.0"><Image/></notxisf>{padding}"#),
        &[],
    ))
    .expect_err("a root element that is not xisf");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(format!("{err}").contains("root element"), "{err}");

    // A tripped guard is the one exception to the class: the file is valid and
    // self-consistent and tripped a configured cap.
    let mut limits = Limits::default();
    limits.xml_header_bytes = 32;
    let err = Reader::seekable_with_limits(
        Cursor::new(Unit::new().image_u16(4, 3, 1, &samples()).build()),
        limits,
    )
    .expect_err("the header cap trips");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("XML header length"), "{err}");
}

/// Row *XISF `embedded` block whose declared digest does not match*: `ChecksumMismatch`, at
/// **construction**, with no `Header`.
///
/// Its contents are read during the header parse, so its digest is verified there. That is
/// what makes tier 1 free for an `attachment` block and not free for an `embedded` one, and it
/// is the only row in the table that fails the whole source over a pixel-block fault.
#[test]
fn an_embedded_block_with_a_bad_digest_is_a_checksum_mismatch_at_construction() {
    let levels = samples();
    let stored = le_u16(&levels);
    // A syntactically valid digest over *different* bytes.
    let wrong = checksum_attr("sha-1", &le_u16(&[9u16; 12]));
    let bytes = Unit::new()
        .xml(&format!(
            r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded" {wrong}><Data encoding="base64">{}</Data></Image>"#,
            base64(&stored)
        ))
        .build();
    let err = seekable(bytes).expect_err("the embedded digest does not match");
    assert_eq!(kind(&err), "ChecksumMismatch", "{err}");

    // The same digest over the right bytes constructs and decodes, so the refusal above is a
    // refusal of the mismatch and not of the attribute.
    let right = checksum_attr("sha-1", &stored);
    let bytes = Unit::new()
        .xml(&format!(
            r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded" {right}><Data encoding="base64">{}</Data></Image>"#,
            base64(&stored)
        ))
        .build();
    let mut reader = seekable(bytes).expect("a matching digest constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let image = reader.read_image().expect("the image decodes");
    assert_same_bits(
        &image.into_samples(),
        &expected_u16(&levels),
        "a verified embedded block",
    );
}

// ------------------------------------------------------------- declined positions

/// Row *Any XISF per-`<Image>` attribute fault*: a declined position **on that image**, per the
/// class rules, reporting full geometry whenever `geometry` reads as a width, a height and a
/// channel count — a zero-length axis included — and `None` when it does not.
#[test]
fn a_per_image_attribute_fault_declines_that_position_with_the_geometry_it_could_read() {
    let full = (Some(4), Some(3), Some(1));
    let none = (None, None, None);

    // Rows whose geometry *reads*, so full geometry is reported however the position is
    // declined. The line is representability, not validity.
    let reads: [(&str, Vec<u8>, DeclineClass, Geometry); 6] = [
        (
            // §8.5.1 calls this an empty image and forbids serializing one — but the three
            // fields read, so all three are reported.
            "a zero-length axis",
            attached(r#"geometry="4:3:0" sampleFormat="UInt16""#),
            DeclineClass::Malformed,
            (Some(4), Some(3), Some(0)),
        ),
        (
            "a complex sample format",
            attached(r#"geometry="4:3:1" sampleFormat="Complex32""#),
            DeclineClass::Unsupported,
            full,
        ),
        (
            "an unrecognized sample format",
            attached(r#"geometry="4:3:1" sampleFormat="Float24""#),
            DeclineClass::Malformed,
            full,
        ),
        (
            "the CIELab colour space",
            attached(r#"geometry="4:3:1" sampleFormat="UInt16" colorSpace="CIELab""#),
            DeclineClass::Unsupported,
            full,
        ),
        (
            "an unrecognized byte order",
            attached(r#"geometry="4:3:1" sampleFormat="UInt16" byteOrder="middle""#),
            DeclineClass::Malformed,
            full,
        ),
        (
            "no location attribute at all",
            unattached(r#"geometry="4:3:1" sampleFormat="UInt16""#),
            DeclineClass::Malformed,
            full,
        ),
    ];
    for (what, bytes, class, want) in reads {
        let (header, raised) = declined(what, bytes);
        assert_eq!(raised, class, "{what}");
        assert_eq!(geometry(&header), want, "{what}");
    }

    // Rows whose geometry does **not** read as a width, a height and a channel count. There is
    // no value to report through unsigned accessors, so the three report `None` as a unit —
    // a partial geometry is not a state this crate produces.
    let unreadable: [(&str, Vec<u8>, DeclineClass); 4] = [
        (
            "a one-dimensional image",
            attached(r#"geometry="12:1" sampleFormat="UInt16""#),
            DeclineClass::Unsupported,
        ),
        (
            "a three-dimensional image",
            attached(r#"geometry="4:3:2:1" sampleFormat="UInt16""#),
            DeclineClass::Unsupported,
        ),
        (
            "a non-numeric axis",
            attached(r#"geometry="4:tall:1" sampleFormat="UInt16""#),
            DeclineClass::Malformed,
        ),
        (
            "a missing geometry",
            attached(r#"sampleFormat="UInt16""#),
            DeclineClass::Malformed,
        ),
    ];
    for (what, bytes, class) in unreadable {
        let (header, raised) = declined(what, bytes);
        assert_eq!(raised, class, "{what}");
        assert_eq!(geometry(&header), none, "{what}");
    }

    // `sample_format` has its own `None` rule, and a complex format is the XISF half of it: the
    // name is recognized and has no representable form in this crate's output.
    let (header, _) = declined(
        "a complex sample format",
        attached(r#"geometry="4:3:1" sampleFormat="Complex32""#),
    );
    assert_eq!(header.sample_format(), None);
    // A declined position reports `WholeImage`, whatever the decline: no delivery is possible
    // there and every pixel call errors anyway.
    assert_eq!(header.granularity(), astroframe::Granularity::WholeImage);
}

// ------------------------------------------------------------------ the validation order

/// § Errors → *Validation order*, graded directly:
///
/// > geometry → `colorSpace` → `sampleFormat` → `byteOrder` → `pixelStorage` → `location`
/// > → `compression` → `offset` → `ColorFilterArray` → `Resolution` → `DisplayFunction`,
/// > first error wins
///
/// The classes depend on it, so each pair below carries **two** faults and asserts which one
/// the caller sees. Where the two faults raise the same class the reason text is asserted
/// instead, since a class assertion could not tell them apart.
#[test]
fn the_header_phase_validation_order_is_first_error_wins_in_the_stated_sequence() {
    // geometry beats colorSpace: an out-of-scope dimensionality is `Unsupported`, an
    // unrecognized colour space is `Malformed`, and the caller sees the first.
    let (_, class) = declined(
        "geometry before colorSpace",
        attached(r#"geometry="4:3:2:1" sampleFormat="UInt16" colorSpace="Sepia""#),
    );
    assert_eq!(class, DeclineClass::Unsupported);

    // colorSpace beats sampleFormat.
    let (_, class) = declined(
        "colorSpace before sampleFormat",
        attached(r#"geometry="4:3:1" colorSpace="CIELab" sampleFormat="Float24""#),
    );
    assert_eq!(class, DeclineClass::Unsupported);

    // sampleFormat beats byteOrder.
    let (_, class) = declined(
        "sampleFormat before byteOrder",
        attached(r#"geometry="4:3:1" sampleFormat="Complex64" byteOrder="middle""#),
    );
    assert_eq!(class, DeclineClass::Unsupported);

    // byteOrder beats pixelStorage — both `Malformed`, so the reason is what distinguishes
    // them.
    let (header, _) = declined(
        "byteOrder before pixelStorage",
        attached(
            r#"geometry="4:3:1" sampleFormat="UInt16" byteOrder="middle" pixelStorage="Woven""#,
        ),
    );
    let reason = header.decline_reason().expect("declined").reason();
    assert!(reason.contains("byteOrder"), "{reason}");

    // pixelStorage beats location.
    let (header, _) = declined(
        "pixelStorage before location",
        unattached(r#"geometry="4:3:1" sampleFormat="UInt16" pixelStorage="Woven""#),
    );
    let reason = header.decline_reason().expect("declined").reason();
    assert!(reason.contains("pixelStorage"), "{reason}");

    // location beats compression: a missing location is `Malformed` and an unknown codec is
    // `Unsupported`, so the class says which ran first.
    let (_, class) = declined(
        "location before compression",
        unattached(r#"geometry="4:3:1" sampleFormat="UInt16" compression="brotli:24""#),
    );
    assert_eq!(class, DeclineClass::Malformed);

    // compression beats offset.
    let (_, class) = declined(
        "compression before offset",
        attached(r#"geometry="4:3:1" sampleFormat="UInt16" compression="brotli:24" offset="-1""#),
    );
    assert_eq!(class, DeclineClass::Unsupported);

    // offset beats the ancillary elements — both `Malformed`, so the reason distinguishes
    // them. Every attribute is settled before the first child element is classified.
    let bytes = Unit::new()
        .attached(
            concat!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" offset="-1" {loc}>"#,
                r#"<Resolution horizontal="-5" vertical="300"/></Image>"#,
            ),
            le_u16(&repeating_u16(12)),
        )
        .build();
    let (header, _) = declined("offset before Resolution", bytes);
    let reason = header.decline_reason().expect("declined").reason();
    assert!(reason.contains("offset"), "{reason}");

    // Among the three the order is by element name and never by document position: §11.5
    // leaves the order of an image's children free, so a file writing its `DisplayFunction`
    // first must classify exactly as one writing its `ColorFilterArray` first does.
    for (what, children) in [
        (
            "ColorFilterArray before Resolution",
            concat!(
                r#"<Resolution horizontal="-5" vertical="300"/>"#,
                r#"<ColorFilterArray width="2" height="2"/>"#,
            ),
        ),
        (
            "Resolution before DisplayFunction",
            concat!(
                r#"<DisplayFunction m="0.5:0.5" s="0:0:0:0" h="1:1:1:1" l="0:0:0:0" r="1:1:1:1"/>"#,
                r#"<Resolution horizontal="-5" vertical="300"/>"#,
            ),
        ),
    ] {
        let (header, _) = declined(what, with_children(children));
        let reason = header.decline_reason().expect("declined").reason();
        let first = what.split(' ').next().expect("the element name");
        assert!(reason.contains(first), "{what}: {reason}");
    }

    // And all three beat `checksum`, which is not one of the positions the order names: an
    // unreadable element is `Malformed` and an unknown digest algorithm is `Unsupported`, so
    // the class says which ran first.
    let bytes = Unit::new()
        .attached(
            concat!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" checksum="md5:00" {loc}>"#,
                r#"<Resolution horizontal="-5" vertical="300"/></Image>"#,
            ),
            le_u16(&repeating_u16(12)),
        )
        .build();
    let (_, class) = declined("Resolution before checksum", bytes);
    assert_eq!(class, DeclineClass::Malformed);

    // **The case the order exists for.** An unsupported *attribute* and no `location` at all
    // must yield `Unsupported`, not `Malformed` — and does so only because the location check
    // runs last. Validating `location` early, the natural instinct since it drives the read,
    // reclassifies every fixture of this shape.
    for attrs in [
        r#"geometry="4:3:2:1" sampleFormat="UInt16""#,
        r#"geometry="4:3:1" sampleFormat="UInt16" colorSpace="CIELab""#,
        r#"geometry="4:3:1" sampleFormat="Complex32""#,
    ] {
        let (_, class) = declined(
            "an unsupported attribute and no location",
            unattached(attrs),
        );
        assert_eq!(class, DeclineClass::Unsupported, "{attrs}");
    }
}

// -------------------------------------------------------------- the ancillary elements

/// Row *… or an unreadable `ColorFilterArray`, `Resolution` or `DisplayFunction` child*:
/// `Malformed`, at a declined position, with the geometry reported in full.
///
/// The three elements are reported and applied to nothing, which is why the temptation is to
/// repair them — and § The organizing principle is what rules that out. Each case below is one
/// the crate could answer with a value the file never wrote.
#[test]
fn an_unreadable_ancillary_element_declines_rather_than_repairing_the_file() {
    // `Resolution`, in both halves of "unreadable": a value that does not parse, and one that
    // parses and is not a resolution (§11.11.1 requires each axis to be greater than zero).
    // 72.0 reported for either of them is a figure the file never wrote, beside the 300.0 it
    // did.
    for (what, element) in [
        (
            "a horizontal that does not parse",
            r#"<Resolution horizontal="not-a-number" vertical="300"/>"#,
        ),
        (
            "a negative horizontal",
            r#"<Resolution horizontal="-5" vertical="300"/>"#,
        ),
        (
            "a zero vertical",
            r#"<Resolution horizontal="300" vertical="0"/>"#,
        ),
        (
            "a NaN vertical, which §8.3.3 spells",
            r#"<Resolution horizontal="300" vertical="NaN"/>"#,
        ),
        (
            "no horizontal at all, which §11.11.1 requires",
            r#"<Resolution vertical="300"/>"#,
        ),
    ] {
        let (header, class) = declined(what, with_children(element));
        assert_eq!(class, DeclineClass::Malformed, "{what}");
        assert_eq!(geometry(&header), (Some(4), Some(3), Some(1)), "{what}");
        let reason = header.decline_reason().expect("declined").reason();
        assert!(reason.contains("Resolution"), "{what}: {reason}");
    }

    // A parseable value is reported verbatim beside its decline — the file did state it — and
    // the specification's default stands in only where there is no number to state.
    let (header, _) = declined(
        "the reported pair",
        with_children(r#"<Resolution horizontal="-5" vertical="300"/>"#),
    );
    let resolution = header.resolution().expect("XISF defines a resolution");
    assert_eq!(
        (resolution.horizontal(), resolution.vertical()),
        (-5.0, 300.0),
        "the stated figures, not the 72.0 default"
    );

    // `ColorFilterArray` is the worst of the three to drop: `cfa()` reporting `None` is the
    // positive claim *this image is not mosaiced*, so a consumer branching on it to decide
    // whether to debayer would be told the opposite of what a one-shot-colour frame says.
    for (what, element) in [
        (
            "a width that does not parse",
            r#"<ColorFilterArray pattern="GRBG" width="two" height="2"/>"#,
        ),
        (
            "no pattern, which §11.10.1 requires",
            r#"<ColorFilterArray width="2" height="2"/>"#,
        ),
        (
            "no height, which §11.10.1 requires",
            r#"<ColorFilterArray pattern="GRBG" width="2"/>"#,
        ),
    ] {
        let (header, class) = declined(what, with_children(element));
        assert_eq!(class, DeclineClass::Malformed, "{what}");
        assert_eq!(geometry(&header), (Some(4), Some(3), Some(1)), "{what}");
        let reason = header.decline_reason().expect("declined").reason();
        assert!(reason.contains("ColorFilterArray"), "{what}: {reason}");
        // `cfa()` is `None` here, and it is the decline rather than that `None` that carries
        // the answer: absence of the element is what means "not mosaiced".
        assert!(header.cfa().is_none(), "{what}");
    }

    // `DisplayFunction`: §11.9.1 requires all five parameters, each written as four fields.
    for (what, element) in [
        (
            "no r at all",
            r#"<DisplayFunction m="0.5:0.5:0.5:0.5" s="0:0:0:0" h="1:1:1:1" l="0:0:0:0"/>"#,
        ),
        (
            "an m of two fields",
            r#"<DisplayFunction m="0.5:0.5" s="0:0:0:0" h="1:1:1:1" l="0:0:0:0" r="1:1:1:1"/>"#,
        ),
        (
            "an s field that does not parse",
            r#"<DisplayFunction m="0.5:0.5:0.5:0.5" s="x:0:0:0" h="1:1:1:1" l="0:0:0:0" r="1:1:1:1"/>"#,
        ),
    ] {
        let (header, class) = declined(what, with_children(element));
        assert_eq!(class, DeclineClass::Malformed, "{what}");
        assert_eq!(geometry(&header), (Some(4), Some(3), Some(1)), "{what}");
        let reason = header.decline_reason().expect("declined").reason();
        assert!(reason.contains("DisplayFunction"), "{what}: {reason}");
    }
}

/// One root-level element reached by several `<Reference>` elements is read once for the whole
/// document, and its fault reaches **every** image that references it.
///
/// The memo is what this grades: one holding the read value alone would decline the first
/// image and hand the second a repaired element, which is the same fabrication by another
/// route.
#[test]
fn an_unreadable_referenced_element_declines_every_image_that_reaches_it() {
    let bytes = Unit::new()
        .xml(r#"<Resolution uid="r" horizontal="-5" vertical="300"/>"#)
        .attached(
            r#"<Image geometry="4:3:1" sampleFormat="UInt16" {loc}><Reference ref="r"/></Image>"#,
            le_u16(&repeating_u16(12)),
        )
        .attached(
            r#"<Image geometry="4:3:1" sampleFormat="UInt16" {loc}><Reference ref="r"/></Image>"#,
            le_u16(&repeating_u16(12)),
        )
        .build();

    let mut reader = seekable(bytes).expect("the unit constructs");
    for position in 0..2 {
        assert!(
            reader.next_image().expect("the walk advances"),
            "{position}"
        );
        let header = reader.header().expect("a declined position still reports");
        let decline = header
            .decline_reason()
            .unwrap_or_else(|| panic!("image {position} declines"));
        assert_eq!(decline.class(), DeclineClass::Malformed, "{position}");
        assert!(decline.reason().contains("Resolution"), "{position}");
    }
    assert!(!reader.next_image().expect("the walk ends"));
}

// ------------------------------------------------------------------ the Neither row

/// Row *XISF unit whose walk finds no image occurrence*: **neither** a construction failure nor
/// a `next_image()` failure. The walk ends normally with no error and `header()` is `None`.
///
/// The same answer the FITS side gives a `NAXIS = 0` primary, and deliberately so: a decoder
/// whose two formats disagreed about "this file holds no images" is the inconsistency this
/// design exists to prevent.
#[test]
fn a_unit_declaring_no_image_walks_zero_images_and_ends_normally() {
    let bytes = Unit::new()
        .xml(r#"<Metadata><Property id="XISF:CreatorApplication" type="String" value="a test"/></Metadata>"#)
        // A `Thumbnail` is not an image this crate reports, and neither is a `Reference` that
        // resolves to something other than an `Image`.
        .xml(r#"<Property uid="p1" id="Root:Loose" type="String" value="x"/>"#)
        .xml(r#"<Reference ref="p1"/>"#)
        .build();

    let mut reader = seekable(bytes).expect("a unit declaring no image still constructs");
    assert!(reader.header().is_none(), "construction selects no image");
    assert!(
        !reader.next_image().expect("end of source is not an error"),
        "the walk ends normally rather than erroring"
    );
    assert!(reader.header().is_none(), "and reports no Header");
    // Idempotent: a second advance is still `Ok(false)`.
    assert!(!reader.next_image().expect("still not an error"));
}

// ------------------------------- the multi-image half of *Multi-image and source-mode*

/// A unit using §11.13's deduplicated spelling: one `<Image uid>` plus two bare root-level
/// `<Reference>` elements, which the specification calls achieving "the same result … in a much
/// cleaner way".
fn deduplicated_unit(levels: &[u16]) -> Unit {
    Unit::new()
        .attached(
            r#"<Image uid="master" geometry="4:3:1" sampleFormat="UInt16" {loc}/>"#,
            le_u16(levels),
        )
        .xml(r#"<Reference ref="master"/>"#)
        .xml(r#"<Reference ref="master"/>"#)
}

/// The deduplicated spelling reports **one occurrence per `Reference`**, not one per `Image`
/// element.
///
/// A decoder walking only `Image` elements would report one image on that conforming file,
/// silently — which is the loss this design refuses. All four occurrences share a single
/// attachment offset, so a seekable source re-reads that block per occurrence.
#[test]
fn the_deduplicated_reference_spelling_reports_one_occurrence_per_reference() {
    let levels = samples();
    let want = expected_u16(&levels);
    let mut reader = seekable(deduplicated_unit(&levels).build()).expect("the unit constructs");

    let mut seen = 0;
    while reader.next_image().expect("the walk advances") {
        seen += 1;
        let header = reader.header().expect("a header per occurrence");
        // Every occurrence is the same image, so it reports the same identity and the same
        // geometry — a `Reference` is the image, not a copy of it.
        assert_eq!(header.image_id(), None, "occurrence {seen}");
        assert_eq!(geometry(&header), (Some(4), Some(3), Some(1)));
        let image = reader.read_image().expect("each occurrence decodes");
        assert_same_bits(&image.into_samples(), &want, &format!("occurrence {seen}"));
    }
    assert_eq!(seen, 3, "one Image element plus two root-level References");
}

/// The source-mode half of the same file: on a **sequential** source the second occurrence is
/// `Unsupported`, its block lying behind the cursor, while the same bytes decode fully through
/// `Reader::seekable`.
///
/// `Unsupported` rather than `Malformed`: the file is valid and self-consistent and is asking
/// something of the *source* it cannot do.
#[test]
fn a_second_occurrence_of_one_block_is_unsupported_on_a_sequential_source_only() {
    let levels = samples();
    let want = expected_u16(&levels);
    let bytes = deduplicated_unit(&levels).build();

    let mut reader = sequential(bytes.clone()).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let image = reader.read_image().expect("the first occurrence decodes");
    assert_same_bits(&image.into_samples(), &want, "sequential, first occurrence");

    // The walk itself does not fail — the position is reachable, the *block* is not.
    assert!(reader.next_image().expect("the walk still advances"));
    assert!(
        reader
            .header()
            .expect("a header")
            .decline_reason()
            .is_none(),
        "nothing about the header declines it; the source is what cannot go back"
    );
    // A correctly sized destination, since this position is not declined and the length check
    // would otherwise mask the answer with an `InvalidRequest`.
    let err = reader
        .read_image_into(&mut [0.0; 12])
        .expect_err("the block lies behind the cursor");
    assert_eq!(kind(&err), "Unsupported", "{err}");
    assert!(format!("{err}").contains("behind"), "{err}");

    // The same bytes through a seekable source decode every occurrence, which is what makes
    // the refusal above a statement about the source rather than about the file.
    let mut reader = seekable(bytes).expect("the unit constructs");
    for occurrence in 1..=3 {
        assert!(reader.next_image().expect("the walk advances"));
        let image = reader
            .read_image()
            .expect("seekable decodes every occurrence");
        assert_same_bits(
            &image.into_samples(),
            &want,
            &format!("seekable, occurrence {occurrence}"),
        );
    }
    assert!(!reader.next_image().expect("the walk ends"));
}

// The per-image reset of `with_bounds` and `select_channel` is graded in
// `pipeline::with_bounds_and_select_channel_reset_across_next_image`, over the same multi-image
// XISF shape and with the assertion that decides it: the cleared override changes the decoded
// pixels rather than only the reported range.

/// A per-image attribute fault declines **that image** without failing the source.
///
/// Header-phase attribute validation is per-`<Image>`, so a fault in one element is a declined
/// position on that image. The corpus makes it concrete: one master holds two images of
/// different geometry *and* different sample
/// format in one file — so a source may legitimately hold one image this version cannot read
/// beside others it can, and aborting the whole walk over the first would be the wrong outcome
/// for a batch consumer.
#[test]
fn one_faulty_image_declines_its_own_position_and_the_others_still_decode() {
    let good = repeating_u16(12);
    let bytes = Unit::new()
        .attached(
            r#"<Image id="a" geometry="4:3:1" sampleFormat="UInt16" {loc}/>"#,
            le_u16(&good),
        )
        .attached(
            // Recognized and declined; the block beside it is perfectly good.
            r#"<Image id="b" geometry="4:3:1" sampleFormat="Complex32" {loc}/>"#,
            le_u16(&good),
        )
        .attached(
            r#"<Image id="c" geometry="4:3:1" sampleFormat="UInt16" {loc}/>"#,
            le_u16(&good),
        )
        .build();

    let want = expected_u16(&good);
    let mut reader = seekable(bytes).expect("one faulty image does not fail the source");

    assert!(reader.next_image().expect("the walk advances"));
    assert_eq!(reader.header().expect("a header").image_id(), Some("a"));
    let image = reader.read_image().expect("the first image decodes");
    assert_same_bits(&image.into_samples(), &want, "image a");

    assert!(
        reader
            .next_image()
            .expect("the walk advances past the fault")
    );
    let header = reader.header().expect("a declined position still reports");
    assert_eq!(header.image_id(), Some("b"));
    let decline = header.decline_reason().expect("declined");
    assert_eq!(decline.class(), DeclineClass::Unsupported, "{decline:?}");
    let err = pixel_calls_fail(&mut reader);
    assert_eq!(kind(&err), "Unsupported", "{err}");

    // The decline did not consume the walk: XISF blocks are located by declared offset rather
    // than by walking past them, so a declined image never blocks the next one.
    assert!(reader.next_image().expect("the walk advances"));
    assert_eq!(reader.header().expect("a header").image_id(), Some("c"));
    let image = reader.read_image().expect("the third image decodes");
    assert_same_bits(&image.into_samples(), &want, "image c");

    assert!(!reader.next_image().expect("the walk ends"));
}
