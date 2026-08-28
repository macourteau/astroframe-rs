//! § XISF decisions, graded row by row through the **public API**, plus the XISF halves of
//! *Reported metadata is reachable*, *Metadata that has no FITS equivalent survives*,
//! *`granularity()` reports the right value*, *Baseline XISF decoder conformance* and
//! *Header-only decode works on a truncated prefix*.
//!
//! Everything here drives `Reader` → `next_image` → `header()` → pixels. `src/xisf/image.rs`
//! already unit-tests the header walk directly; where a unit test pins a mapping, the test
//! here pins the observable end-to-end consequence instead — decoded samples, compared with
//! `f32::to_bits()`.
//!
//! Fixtures are synthetic and built byte by byte. Nothing here carries a real frame header.
//!
//! Three of these tests were written against **library defects** this suite exposed, to the
//! design rather than to the implementation, and each turned green when its fix landed:
//! `a_subblocked_zlib_block_streams_by_rows` (each subblock is an independently
//! compressed stream, so the codec restarts at every boundary),
//! `a_root_element_in_another_namespace_is_malformed_at_construction`, and
//! `a_declared_non_utf8_header_encoding_is_unsupported`. Writing a test to the design and
//! letting it fail is what made the fixes cheap to land; none needed rediscovering.

#![forbid(unsafe_code)]

mod common;

use std::io::Cursor;

use astroframe::{
    ColorSpace, DeclineClass, Granularity, Header, ImageType, KeywordOrigin, Orientation,
    PixelStorage, Property, PropertyScope, PropertyType, PropertyValue, Reader, SampleFormat,
    Samples, Seekable, Sequential,
};
use common::xisf::{
    PREAMBLE, Unit, base64, be_u16, checksum_attr, expected_u16, hex, le_f32, le_u8, le_u16, lz4,
    repeating_u16, samples, shuffle, with_header, zlib,
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

/// `<Image>` over the standard fixture geometry, with `extra` attributes spliced in.
fn image_element(extra: &str) -> String {
    format!(r#"<Image geometry="4:3:1" sampleFormat="UInt16" {extra} {{loc}}/>"#)
}

/// A one-image unit whose block is attached, with `extra` attributes on the `<Image>`.
fn attached_u16(extra: &str, stored: Vec<u8>) -> Vec<u8> {
    Unit::new().attached(&image_element(extra), stored).build()
}

/// Advance to the one image a fixture holds and decode it.
fn read_one(bytes: Vec<u8>) -> (Header, Vec<f32>) {
    let mut reader = seekable(bytes).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"), "one image");
    let header = reader.header().expect("an advanced reader has a header");
    let image = reader.read_image().expect("the image decodes");
    assert!(
        !reader.next_image().expect("the walk ends"),
        "the fixture holds exactly one image"
    );
    (header, image.into_samples())
}

/// Advance to the one image a fixture holds, decode it, and check it against the fixture's
/// own stored levels.
fn decodes_to(bytes: Vec<u8>, levels: &[u16], what: &str) -> Header {
    let (header, got) = read_one(bytes);
    assert_same_bits(&got, &expected_u16(levels), what);
    header
}

/// Build a unit whose single attachment writes its **own** `location` spelling.
///
/// `Unit` always writes `attachment:`, and both the `attached:` alternate and a deliberately
/// wrong position are decisions in their own right, so the fixed point is iterated here.
/// `render` receives the position and size the builder settled on and returns the whole
/// header text.
fn attached_unit(render: impl Fn(u64, u64) -> String, stored: &[u8]) -> Vec<u8> {
    let size = stored.len() as u64;
    let mut position = 0u64;
    let mut header = String::new();
    let mut converged = false;
    for _ in 0..16 {
        header = render(position, size);
        let next = PREAMBLE as u64 + header.len() as u64;
        if next == position {
            converged = true;
            break;
        }
        position = next;
    }
    // The same trap `tests/common/xisf.rs` documents: the position's digit count feeds back
    // into the header length that determines the position.
    assert!(converged, "attachment offsets did not converge");
    with_header(&header, stored)
}

/// The default root attributes — the namespace §9.5 says a header *should* carry.
const ROOT: &str = r#" xmlns="http://www.pixinsight.com/xisf" version="1.0""#;

/// Wrap a root element's children in the declaration and root the builder writes.
fn unit_header(body: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><xisf{ROOT}>{body}</xisf>")
}

/// A one-image unit over the standard fixture geometry whose `location` attribute is spelled
/// by the caller, from the position and size the builder settled on.
fn located_u16(location: impl Fn(u64, u64) -> String, stored: &[u8]) -> Vec<u8> {
    attached_unit(
        |position, size| {
            unit_header(&format!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="{}"/>"#,
                location(position, size)
            ))
        },
        stored,
    )
}

/// A zstd frame built from **raw** (stored) blocks.
///
/// `zstd` appears nowhere in XISF 1.0 and this crate's support for it is corpus-derived, so
/// the fixture is a frame written here byte by byte rather than one produced by an encoder
/// the crate does not depend on: magic, a single-segment frame header with a one-byte content
/// size, then one last raw block. It exercises exactly what the decision is about — that a
/// `zstd` block is *framed* and is fed to a streaming decoder.
fn zstd_raw(input: &[u8]) -> Vec<u8> {
    assert!(input.len() < 256, "the one-byte frame content size field");
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd];
    out.push(0x20); // Single_Segment_flag, so the window is the content size
    out.push(input.len() as u8);
    let block_header: u32 = ((input.len() as u32) << 3) | 1; // last block, Raw_Block
    out.extend_from_slice(&block_header.to_le_bytes()[..3]);
    out.extend_from_slice(input);
    out
}

// -------------------------------------------------- the preamble and the header region

/// Row *The preamble's four reserved bytes are ignored, and the 65-byte minimum header
/// length is enforced*.
#[test]
fn the_reserved_bytes_are_ignored_and_the_minimum_header_length_is_enforced() {
    let levels = samples();
    let mut bytes = attached_u16("", le_u16(&levels));
    // §9.2's "shall be zero" binds encoders and places no obligation on readers, so a unit
    // whose reserved field is garbage decodes exactly as one whose field is zero.
    bytes[12..16].copy_from_slice(&[0xff, 0xfe, 0xfd, 0xfc]);
    decodes_to(bytes, &levels, "reserved bytes ignored");

    // The other half of the row: the specification's own validity check inspects the
    // signature and this minimum, and 64 is one byte short of it.
    let err = seekable(with_header(&"x".repeat(64), &[])).expect_err("a 64-byte header");
    assert_eq!(kind(&err), "Malformed", "{err}");
    assert!(
        format!("{err}").contains("65"),
        "the reason names the floor: {err}"
    );
}

/// Row *The XML header may not be a well-formed standalone document, and that is fine*.
///
/// A signed unit places its `<Signature>` **after** `</xisf>` and inside the declared header
/// length (§9.5), so the header buffer has two roots. Pull-parsing stops caring once the
/// `xisf` element closes, which is what makes such a unit decode normally.
#[test]
fn a_signed_unit_decodes_although_its_header_carries_two_roots() {
    let levels = samples();
    let bytes = attached_unit(
        |position, size| {
            format!(
                "{}<Signature xmlns=\"http://www.w3.org/2000/09/xmldsig#\">\
                 <Reference URI=\"\"><DigestValue>QUJD</DigestValue></Reference></Signature>",
                unit_header(&format!(
                    r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="attachment:{position}:{size}"/>"#
                ))
            )
        },
        &le_u16(&levels),
    );
    let header = decodes_to(bytes, &levels, "signed unit");
    // Parsing stopped at the first `</xisf>`, so the XML-DSig subtree's own `Reference`
    // element was never reached — which is what keeps it from being confused with §11.13's.
    assert!(header.keywords().is_empty());
    assert!(header.properties().is_empty());
}

/// Row *Header encoding is UTF-8 … A missing XML declaration is tolerated*, and row
/// *`Metadata`'s absence is tolerated*.
#[test]
fn a_missing_xml_declaration_and_a_missing_metadata_element_are_both_tolerated() {
    let levels = samples();
    // No `<?xml…?>` — which makes the header invalid by §9.5 and is tolerated anyway, as
    // deliberate leniency toward real writers. And no `<Metadata>`, which the specification
    // requires and for which it defines no failure mode.
    let bytes = attached_unit(
        |position, size| {
            format!(
                r#"<xisf{ROOT}><Image geometry="4:3:1" sampleFormat="UInt16" location="attachment:{position}:{size}"/></xisf>"#
            )
        },
        &le_u16(&levels),
    );
    decodes_to(bytes, &levels, "no declaration, no Metadata");
}

// ------------------------------------------------------------------ location spellings

/// Row *Both `attachment:` and `attached:` are accepted*.
///
/// Four of the specification's own examples write `attached:`, and a writer that followed
/// them produces files that are otherwise valid.
#[test]
fn the_attached_alternate_location_spelling_decodes_identically() {
    let levels = samples();
    let stored = le_u16(&levels);
    let normative = located_u16(|p, s| format!("attachment:{p}:{s}"), &stored);
    let alternate = located_u16(|p, s| format!("attached:{p}:{s}"), &stored);
    // Different header lengths, so the two units are not byte-identical; their pixels are.
    let (_, from_normative) = read_one(normative);
    let (_, from_alternate) = read_one(alternate);
    assert_same_bits(
        &from_alternate,
        &expected_u16(&levels),
        "attached: spelling",
    );
    assert_same_bits(&from_alternate, &from_normative, "the two spellings agree");
}

/// Row *An `attachment` position must lie beyond the header region*.
///
/// A declared position of 40 on a seekable source would otherwise hand the caller the XML
/// header's own bytes as pixel samples: a decode that looks plausible and is fabricated.
#[test]
fn an_attachment_position_inside_the_header_region_is_refused() {
    let levels = samples();
    let stored = le_u16(&levels);
    let inside = located_u16(|_p, s| format!("attachment:40:{s}"), &stored);
    let mut reader = seekable(inside).expect("the unit itself is well-formed");
    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("a declined position still reports");
    let decline = header.decline_reason().expect("the position is declined");
    assert_eq!(decline.class(), DeclineClass::Malformed);
    assert!(
        decline.reason().contains("header region"),
        "{}",
        decline.reason()
    );
    // The geometry read, so it is reported; only the position is refused.
    assert_eq!(
        (header.width(), header.height(), header.channels()),
        (Some(4), Some(3), Some(1))
    );
    assert_eq!(header.granularity(), Granularity::WholeImage);
    let err = reader
        .read_image_into(&mut [0.0; 12])
        .expect_err("a refused position decodes nothing");
    assert_eq!(kind(&err), "Malformed", "{err}");

    // The boundary case, one byte the other way: the first position beyond the header
    // region is accepted, which is what shows the comparison is not off by one.
    let ok = located_u16(|p, s| format!("attachment:{p}:{s}"), &stored);
    decodes_to(ok, &levels, "the first position past the header");
}

// ------------------------------------------------------------------ byte order

/// Row *`byteOrder` absent means little-endian* (§10.4), and the `big` spelling.
///
/// This is the document's own highest-risk pinned default: a wrong guess corrupts every
/// sample silently rather than erroring, so both spellings are decoded to asserted values
/// and compared against each other.
#[test]
fn big_endian_blocks_decode_and_absent_means_little_endian() {
    let levels = samples();
    // The cycle includes 257 and 65535, whose two bytes differ, so a decoder that ignored
    // `byteOrder` would produce visibly different levels rather than agreeing by accident.
    let big = attached_u16(r#"byteOrder="big""#, be_u16(&levels));
    let (_, from_big) = read_one(big);
    assert_same_bits(&from_big, &expected_u16(&levels), "byteOrder=\"big\"");

    let little = attached_u16(r#"byteOrder="little""#, le_u16(&levels));
    let (_, from_little) = read_one(little);
    let absent = attached_u16("", le_u16(&levels));
    let (_, from_absent) = read_one(absent);
    assert_same_bits(&from_little, &from_absent, "absent means little-endian");
    assert_same_bits(&from_big, &from_absent, "the two byte orders agree");
}

// ------------------------------------------------------------------ embedded blocks

/// A unit whose whole content is header XML — every `embedded` fixture has this shape.
fn xml_unit(body: &str) -> Vec<u8> {
    Unit::new().xml(body).build()
}

/// An `<Image location="embedded">` over the standard fixture geometry.
fn embedded_u16(image_extra: &str, data_attrs: &str, text: &str) -> Vec<u8> {
    xml_unit(&format!(
        r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded" {image_extra}><Data {data_attrs}>{text}</Data></Image>"#
    ))
}

/// Row *Embedded blocks come in two encodings, `base64` and lowercase `hex`*.
///
/// The Base16 half is net-new work rather than a port — the prior decoder implements `base64`
/// only — so this test is the only evidence it will get.
#[test]
fn embedded_blocks_decode_from_both_base64_and_lowercase_base16() {
    let levels = samples();
    let stored = le_u16(&levels);

    let from_base64 = embedded_u16("", r#"encoding="base64""#, &base64(&stored));
    let header = decodes_to(from_base64, &levels, "embedded base64");
    // The pixels were fully materialized during the header parse, so no part of the input
    // remains to stream.
    assert_eq!(header.granularity(), Granularity::WholeImage);

    let from_hex = embedded_u16("", r#"encoding="hex""#, &hex(&stored));
    decodes_to(from_hex, &levels, "embedded hex");
}

/// The same row's other half: an uppercase Base16 spelling is rejected rather than accepted
/// leniently, and an unknown encoding is `Malformed`.
#[test]
fn an_uppercase_base16_spelling_and_an_unknown_encoding_are_both_refused() {
    let levels = samples();
    let stored = le_u16(&levels);

    let uppercase = embedded_u16("", r#"encoding="hex""#, &hex(&stored).to_uppercase());
    let (class, reason) = declined(uppercase);
    // §10.3 spells Base16 with the lowercase digits and is not silent on case, so there is
    // nothing to guess at.
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(reason.contains("Base16"), "{reason}");

    let unknown = embedded_u16("", r#"encoding="uuencode""#, "AAAA");
    let (class, reason) = declined(unknown);
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(
        reason.contains("base64"),
        "the reason names what it knows: {reason}"
    );
}

/// Row *Whitespace is stripped before Base64/Base16 decode, at the two decode sites and never
/// at the reader*.
///
/// The specification's own embedded example is line-wrapped, so this is a conforming file
/// rather than an edge case.
#[test]
fn white_space_is_stripped_before_a_base64_or_base16_decode() {
    let levels = samples();
    let stored = le_u16(&levels);

    let wrapped = base64(&stored)
        .as_bytes()
        .chunks(8)
        .map(|c| String::from_utf8(c.to_vec()).unwrap())
        .collect::<Vec<_>>()
        .join("\n  ");
    let unit = embedded_u16("", r#"encoding="base64""#, &format!("\n  {wrapped}\n"));
    decodes_to(unit, &levels, "line-wrapped base64");

    let spaced = hex(&stored)
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8(c.to_vec()).unwrap())
        .collect::<Vec<_>>()
        .join(" \t");
    let unit = embedded_u16("", r#"encoding="hex""#, &format!("\r\n{spaced}  "));
    decodes_to(unit, &levels, "white-spaced hex");
}

/// Row *For an embedded block, `compression` and `subblocks` live on the child `<Data>`
/// element*.
///
/// Reading them from the wrong element yields a block that looks uncompressed and decodes to
/// noise — a failure no synthetic round-trip catches unless a fixture exercises
/// embedded-plus-compressed. This is that fixture, and its second half proves the attribute
/// is read from the `<Data>` element rather than merely read from somewhere.
#[test]
fn an_embedded_blocks_compression_is_read_from_the_child_data_element() {
    let levels = samples();
    let stored = le_u16(&levels);
    let compressed = zlib(&stored);

    let right = embedded_u16(
        "",
        &format!(r#"encoding="base64" compression="zlib:{}""#, stored.len()),
        &base64(&compressed),
    );
    decodes_to(right, &levels, "compression on the Data element");

    // The same bytes with the attribute on the `<Image>` instead: the block then looks
    // uncompressed, and an uncompressed block whose length disagrees with the geometry is
    // refused rather than decoded to noise.
    let wrong = embedded_u16(
        &format!(r#"compression="zlib:{}""#, stored.len()),
        r#"encoding="base64""#,
        &base64(&compressed),
    );
    let mut reader = seekable(wrong).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let err = reader
        .read_image_into(&mut [0.0; 12])
        .expect_err("the block does not look like 24 bytes of pixels");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// The decline class and reason at the fixture's first image position, for the fixtures whose
/// point is that the position declines.
fn declined(bytes: Vec<u8>) -> (DeclineClass, String) {
    let mut reader = seekable(bytes).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("a declined position still reports");
    let decline = header
        .decline_reason()
        .expect("the position is declined")
        .clone();
    // Every decline says any pixel call is `Err`, and the class it raises is the class it
    // reported.
    let err = reader
        .read_image_into(&mut [0.0; 12])
        .expect_err("a declined position decodes nothing");
    assert_eq!(kind(&err), format!("{:?}", decline.class()), "{err}");
    (decline.class(), decline.reason().to_owned())
}

// ------------------------------------------------------------- the two text rules

/// A one-image unit whose `<Image>` carries children.
fn attached_u16_with(extra: &str, children: &str, stored: Vec<u8>) -> Vec<u8> {
    Unit::new()
        .attached(
            &format!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" {extra} {{loc}}>{children}</Image>"#
            ),
            stored,
        )
        .build()
}

/// The property with this identifier, which the fixtures below always declare exactly once.
fn property<'a>(header: &'a Header, id: &str) -> &'a Property {
    header
        .properties()
        .iter()
        .find(|p| p.id() == id)
        .unwrap_or_else(|| panic!("the fixture declares {id}: {:?}", header.properties()))
}

fn text_of(property: &Property) -> &str {
    match property.value() {
        PropertyValue::Text(text) => text,
        other => panic!("expected character data, got {other:?}"),
    }
}

/// Row *Whitespace is stripped before Base64/Base16 decode … but §11.1.6 says the opposite
/// for a `String` property's character data*.
///
/// Both surfaces are read by the same parser, so `quick-xml`'s obvious `trim_text(true)`
/// would silently corrupt every string property. One fixture carries both, which is what
/// makes the two rules provably independent rather than separately plausible.
#[test]
fn a_string_propertys_white_space_is_preserved_where_a_blocks_is_stripped() {
    let levels = samples();
    let stored = le_u16(&levels);
    let unit = xml_unit(&format!(
        r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded"><Property id="Observation:Object:Name" type="String">  NGC 7000
</Property><Data encoding="base64">
  {}
</Data></Image>"#,
        base64(&stored)
    ));
    let header = decodes_to(unit, &levels, "block white space stripped");
    assert_eq!(
        text_of(property(&header, "Observation:Object:Name")),
        "  NGC 7000\n",
        "§11.1.6: a compliant decoder must preserve a String property's white space"
    );
}

/// Row *Character data is assembled to the element's end before either rule applies*.
///
/// A run interrupted by an entity reference or a CDATA boundary arrives as several parser
/// events, and a parser reading one text event truncates `String` property values and rejects
/// CDATA-wrapped embedded blocks — both legal XML, and a `String` property containing `<` is
/// exactly why a writer reaches for CDATA.
#[test]
fn character_data_is_assembled_across_cdata_and_entity_boundaries() {
    let levels = samples();
    let stored = le_u16(&levels);
    let encoded = base64(&stored);
    let (head, tail) = encoded.split_at(8);
    let unit = xml_unit(&format!(
        r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded"><Property id="Processing:Description" type="String">a &amp; <![CDATA[b < c]]> d</Property><Data encoding="base64">{head}<![CDATA[{tail}]]></Data></Image>"#
    ));
    let header = decodes_to(unit, &levels, "embedded block split by a CDATA boundary");
    assert_eq!(
        text_of(property(&header, "Processing:Description")),
        "a & b < c d",
        "the three runs assemble before the white-space rule applies"
    );
}

/// Row *XML entity references are resolved; "verbatim" means after unescaping*.
///
/// A consumer comparing a keyword value against a string should not have to know how the
/// writer chose to escape it. `quick-xml` does not unescape automatically, so this is a
/// decision rather than a default.
#[test]
fn entity_references_are_resolved_before_a_value_is_reported() {
    let levels = samples();
    let header = decodes_to(
        attached_u16_with(
            "",
            r#"<FITSKeyword name="OBJECT" value="'M &amp; M&#39;&#39;s'" comment="a &lt;quoted&gt; name"/><Property id="Processing:Tool" type="String" value="&#65;&amp;B"/>"#,
            le_u16(&levels),
        ),
        &levels,
        "entity references",
    );
    let keyword = header.get("OBJECT").expect("the fixture declares OBJECT");
    // The FITS quoting is unwrapped *after* unescaping: the doubled `&#39;&#39;` is one
    // escaped quote per XML and then one literal quote per FITS §4.2.1, and reading the two
    // rules in the other order yields `M & M` and drops everything after it.
    assert_eq!(keyword.value(), "M & M's");
    assert_eq!(keyword.comment(), Some("a <quoted> name"));
    assert_eq!(text_of(property(&header, "Processing:Tool")), "A&B");
}

/// Row *Plain-text scalars follow §8.3*: surrounding white space is ignored (§8.3.4), a
/// leading sign is admitted even where the field is conceptually unsigned (§8.3.1), and `0`,
/// `+0` and `-0` are accepted as integers despite §8.3.1's regex admitting no decimal zero.
#[test]
fn section_8_3_white_space_and_sign_spellings_are_accepted_around_plain_text_scalars() {
    let levels = samples();
    let stored = le_u16(&levels);
    // White space around each numeric field of the location and the geometry — the fields
    // §8.3.1 governs, not the `attachment` keyword §10.3 spells — and a signed zero offset — the
    // spelling `attachment:0:…` makes necessary. `sampleFormat` is *not* among them: §8.3.4
    // governs plain-text scalars, and Table 11's enumeration is not one.
    let unit = attached_unit(
        |position, size| {
            unit_header(&format!(
                r#"<Image geometry=" 4 : 3 : 1 " sampleFormat="UInt16" offset=" +0 " bounds=" 0 : 65535 " location="attachment: {position} : {size} "/>"#
            ))
        },
        &stored,
    );
    let header = decodes_to(unit, &levels, "§8.3.4 white space");
    assert_eq!(header.offset(), Some(0.0));
    // A `bounds` a writer wrote redundantly is `Declared`, not `FormatDefault`: §11.5.1 only
    // says such a `bounds` *should not* be written, so real writers produce it.
    assert_eq!(*header.bounds(), astroframe::Bounds::Declared(0.0, 65535.0));
}

/// Row *The header is parsed namespace-aware, matching elements by local name*.
///
/// §9.5 says the root *should* carry the namespace, so a prefixed serialization is legal and
/// `quick-xml`'s plain reader would fail to match it.
#[test]
fn a_namespace_prefixed_header_decodes() {
    let levels = samples();
    let unit = attached_unit(
        |position, size| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><pi:xisf xmlns:pi="http://www.pixinsight.com/xisf" version="1.0"><pi:Image geometry="4:3:1" sampleFormat="UInt16" location="attachment:{position}:{size}"><pi:FITSKeyword name="EXPTIME" value="120.0" comment="s"/></pi:Image></pi:xisf>"#
            )
        },
        &le_u16(&levels),
    );
    let header = decodes_to(unit, &levels, "namespace-prefixed header");
    // The child elements match by local name too, not just the root.
    assert_eq!(header.get("EXPTIME").map(|k| k.value()), Some("120.0"));
}

/// The other half of the same row: **a root element in some other namespace is `Malformed` at
/// construction**, rather than a document that matches no `Image` and walks zero images —
/// which would be the silent loss this design refuses everywhere.
///
/// `check_root_namespace` in `src/xisf/xml.rs` reads the root's own resolved namespace and
/// rejects a mismatch outright, before any other element of the header is even parsed.
#[test]
fn a_root_element_in_another_namespace_is_malformed_at_construction() {
    let levels = samples();
    let bytes = Unit::new()
        .root_attrs(r#" xmlns="http://example.invalid/not-xisf" version="1.0""#)
        .image_u16(4, 3, 1, &levels)
        .build();
    let err = seekable(bytes).expect_err("a root outside the XISF namespace");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// The rule's descendant half, which the root-only check above cannot exercise: an element
/// bound to some *other* namespace, nested under a root correctly bound to XISF's, is not
/// matched by local name either. Without namespace tracking past the root, `local_name` would
/// strip `ev:Image` down to `Image` and this decodes as an XISF image it plainly is not.
#[test]
fn a_foreign_namespaced_image_element_is_not_an_image_occurrence() {
    let bytes = xml_unit(
        r#"<ev:Image xmlns:ev="http://example.invalid/evil" geometry="2:2:1" sampleFormat="UInt16" location="embedded"/>"#,
    );
    let mut reader = seekable(bytes).expect("the unit constructs");
    assert!(reader.header().is_none(), "construction selects no image");
    assert!(
        !reader.next_image().expect("end of source is not an error"),
        "a foreign-namespaced Image is not walked as an XISF Image"
    );

    // And the same element with the declaration simply left off. A prefix nobody declared is
    // not "no namespace" -- it is namespace-ill-formed XML, and treating it as the unbound
    // case would leave the refusal above trivially bypassable by deleting one attribute.
    let bytes =
        xml_unit(r#"<ev:Image geometry="2:2:1" sampleFormat="UInt16" location="embedded"/>"#);
    let mut reader = seekable(bytes).expect("the unit constructs");
    assert!(
        !reader.next_image().expect("end of source is not an error"),
        "an undeclared prefix is not the unbound case"
    );
}

/// `xmlns=""` **undeclares** the default namespace (XML Namespaces §6.2); it does not bind it
/// to the empty URI. So an element carrying it is the unprefixed-and-unbound case §9.5's
/// "should" tolerates, not a foreign one — read literally the empty string is a URI that is
/// not XISF's, which would have skipped the image and refused the root.
#[test]
fn an_empty_default_namespace_declaration_undeclares_rather_than_binds() {
    let levels = samples();
    let stored = le_u16(&levels);
    let unit = Unit::new()
        .root_attrs(r#" version="1.0" xmlns="""#)
        .xml(&format!(
            r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">{}</Data></Image>"#,
            base64(&stored)
        ))
        .build();
    decodes_to(
        unit,
        &levels,
        "a root that undeclares the default namespace",
    );
}

/// The root half of the same rule. A root outside XISF's namespace is `Malformed` rather than
/// a silent non-match, because matching none of its elements would walk zero images and report
/// a clean empty file — the silent loss this design refuses everywhere. An undeclared prefix
/// on the root is the same answer for the same reason: it is namespace-ill-formed XML, not the
/// unbound root §9.5's "should" permits.
#[test]
fn a_root_outside_the_xisf_namespace_is_malformed_however_it_got_there() {
    // §9.2 sets a 65-byte minimum header, and an under-length header is refused before the
    // XML is looked at: without the padding both fixtures pass for the wrong reason.
    let padding = " ".repeat(64);
    for (case, root, want) in [
        (
            "a foreign default namespace",
            format!(
                r#"<xisf version="1.0" xmlns="http://example.invalid/evil"><Image/></xisf>{padding}"#
            ),
            "namespace",
        ),
        (
            "an undeclared prefix on the root",
            format!(r#"<ev:xisf version="1.0"><Image/></ev:xisf>{padding}"#),
            "prefix",
        ),
    ] {
        let err = seekable(with_header(&root, &[])).expect_err(case);
        assert_eq!(kind(&err), "Malformed", "{case}: {err}");
        assert!(
            format!("{err}").contains(want),
            "{case}: refused for the wrong reason: {err}"
        );
    }
}

/// The unconditional half: an unprefixed `Image`, under a document whose root declares no
/// namespace at all, still decodes. §9.5 only says the namespace *should* be declared, so a
/// header that omits it entirely is conforming and matching cannot require a namespace that
/// was never in scope to begin with.
#[test]
fn an_unprefixed_image_under_a_namespace_free_document_still_decodes() {
    let levels = samples();
    let stored = le_u16(&levels);
    let unit = Unit::new()
        .root_attrs(r#" version="1.0""#)
        .xml(&format!(
            r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">{}</Data></Image>"#,
            base64(&stored)
        ))
        .build();
    decodes_to(
        unit,
        &levels,
        "an unprefixed Image under a namespace-free document",
    );
}

/// The element rule's positive counterpart to `a_foreign_namespaced_image_element_is_not_an_
/// image_occurrence`: a prefix bound to XISF's own namespace — even one the element declares
/// on itself, distinct from the root's default binding — matches exactly like the unprefixed
/// spelling, attribute included (see `a_prefixed_attribute_bound_to_the_xisf_namespace_
/// decodes` below for the attribute half).
#[test]
fn an_element_bound_to_the_xisf_namespace_by_its_own_declaration_decodes() {
    let levels = samples();
    let stored = le_u16(&levels);
    let unit = xml_unit(&format!(
        r#"<pi:Image xmlns:pi="http://www.pixinsight.com/xisf" geometry="4:3:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">{}</Data></pi:Image>"#,
        base64(&stored)
    ));
    decodes_to(
        unit,
        &levels,
        "an Image bound to the XISF namespace by its own xmlns:pi",
    );
}

/// Row *Attributes never inherit a default namespace*, corrected: a prefix on an *attribute*
/// is stripped when — and only when — it resolves into the XISF namespace, the same rule
/// element names follow. `<pi:Image pi:geometry="…">` with `pi` bound to XISF's namespace is
/// spelling the same `geometry` §11.5.1 makes mandatory, not a foreign attribute; keying on the
/// full qualified name unconditionally would make it unreadable.
#[test]
fn a_prefixed_attribute_bound_to_the_xisf_namespace_decodes() {
    let levels = samples();
    let stored = le_u16(&levels);
    let unit = xml_unit(&format!(
        r#"<pi:Image xmlns:pi="http://www.pixinsight.com/xisf" pi:geometry="4:3:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">{}</Data></pi:Image>"#,
        base64(&stored)
    ));
    decodes_to(
        unit,
        &levels,
        "a pi:geometry attribute with pi bound to the XISF namespace",
    );
}

// ------------------------------------------------------------------ §11.13 references

/// Row *`Reference` elements are resolved by `uid` lookup over the whole parsed header, in a
/// second pass* — with its forward-reference case, its pinned order, and a `Reference` to a
/// nonexistent `uid`.
///
/// Forward references are legal: §11.13 requires only that the target be defined in the same
/// unit, and the specification's own examples define the target *after* the `Reference`, so a
/// one-pass backward-only resolver would silently drop metadata on conforming files. Every
/// target here is declared after the `<Image>` that reaches it.
#[test]
fn references_resolve_forward_and_take_the_position_of_the_reference() {
    let levels = samples();
    let unit = Unit::new()
        .attached(
            &format!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" {{loc}}>{}</Image>"#,
                concat!(
                    r#"<FITSKeyword name="FIRST" value="1" comment="own"/>"#,
                    r#"<Reference ref="k2"/>"#,
                    r#"<FITSKeyword name="THIRD" value="3" comment="own"/>"#,
                    r#"<Reference ref="p1"/>"#,
                    r#"<Property id="Image:Own" type="Int32" value="7"/>"#,
                    // A `Reference` to a `uid` no element carries is ignored: the frame
                    // decodes and nothing is raised, an element never failing a frame it does
                    // not prevent decoding.
                    r#"<Reference ref="nothing-declares-this"/>"#,
                ),
            ),
            le_u16(&levels),
        )
        // Every target is declared *after* the image that references it.
        .xml(r#"<FITSKeyword uid="k2" name="SECOND" value="2" comment="referenced"/>"#)
        .xml(r#"<Property uid="p1" id="Root:Referenced" type="String" value="rooted"/>"#)
        // Unreferenced root-level metadata is attached to no image, and reporting it against
        // an arbitrary one would invent an association the file does not make.
        .xml(r#"<FITSKeyword name="ORPHAN" value="0" comment="unreferenced"/>"#)
        .xml(r#"<Property id="Root:Orphan" type="String" value="unreferenced"/>"#)
        .xml(r#"<Metadata><Property id="Meta:Scoped" type="String" value="metadata"/></Metadata>"#)
        .build();
    let header = decodes_to(unit, &levels, "reference resolution");

    // The pinned order: document order, with a referenced element taking the position of its
    // `Reference` — not appended at the end, which is the other obvious choice.
    let keywords: Vec<(&str, KeywordOrigin)> = header
        .keywords()
        .iter()
        .map(|k| (k.name(), k.origin()))
        .collect();
    assert_eq!(
        keywords,
        vec![
            ("FIRST", KeywordOrigin::Image),
            ("SECOND", KeywordOrigin::Reference),
            ("THIRD", KeywordOrigin::Image),
        ]
    );
    // §11.6.1 makes `comment` mandatory, so it is always present for an XISF-sourced keyword.
    assert!(header.keywords().iter().all(|k| k.comment().is_some()));

    let properties: Vec<(&str, PropertyScope)> = header
        .properties()
        .iter()
        .map(|p| (p.id(), p.scope()))
        .collect();
    assert_eq!(
        properties,
        vec![
            // The `Metadata` element sits last in the document and its properties still come
            // first: node indices are assigned in document order and `<Metadata>`'s child is
            // ordered by its own position, which here follows the image's children. This
            // asserts what the merge actually produces rather than what it might.
            ("Root:Referenced", PropertyScope::Image),
            ("Image:Own", PropertyScope::Image),
            ("Meta:Scoped", PropertyScope::Metadata),
        ],
        "a root-level property attached by Reference is tagged with the scope of the element \
         it attaches to, not the root"
    );
}

// ------------------------------------------------------------------ shared builders

/// A one-image unit whose `<Image>` is written attribute by attribute, for the fixtures whose
/// point is a geometry or a sample format other than the standard one.
fn attached_image(attrs: &str, stored: Vec<u8>) -> Vec<u8> {
    Unit::new()
        .attached(&format!("<Image {attrs} {{loc}}/>"), stored)
        .build()
}

/// The header at the fixture's first image position, whatever it reports.
fn first_header(bytes: Vec<u8>) -> Header {
    let mut reader = seekable(bytes).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    reader.header().expect("an advanced reader has a header")
}

/// The geometry three, as a triple, so the representability rule is one assertion.
fn geometry_of(header: &Header) -> (Option<u32>, Option<u32>, Option<u32>) {
    (header.width(), header.height(), header.channels())
}

/// The pinned normalization form for a `UInt8` image at the format default range.
fn expected_u8(levels: &[u8]) -> Vec<f32> {
    levels
        .iter()
        .map(|&l| (l as f64 - 0.0) as f32 * (1.0f32 / 255.0f32))
        .collect()
}

/// The pinned form for a `Float32` image over a declared `bounds` of `0:1`.
fn expected_f32(levels: &[f32], lo: f64, hi: f64) -> Vec<f32> {
    let k = 1.0f32 / ((hi - lo) as f32);
    levels
        .iter()
        .map(|&s| {
            let shifted = ((s as f64) - lo) as f32;
            let out = shifted * k;
            out.clamp(0.0, 1.0)
        })
        .collect()
}

/// Split a block in two and compress each half **independently**, which is what §10.6's
/// `subblocks` split is: a split over the compression, not over the stored block.
///
/// Returns the concatenated stored bytes and the `subblocks` attribute text.
fn subblocked(plain: &[u8], compress: impl Fn(&[u8]) -> Vec<u8>) -> (Vec<u8>, String) {
    let (a, b) = plain.split_at(plain.len() / 2);
    let (ca, cb) = (compress(a), compress(b));
    let mut stored = ca.clone();
    stored.extend_from_slice(&cb);
    let attr = format!("{},{}:{},{}", ca.len(), a.len(), cb.len(), b.len());
    (stored, attr)
}

// ------------------------------------------------------------------ geometry and location

/// Row *Geometry is `dim_1:…:dim_N:channel-count`, and this version supports N = 2 exactly*,
/// and row *A geometry with any zero-length axis is `Malformed`*.
///
/// Distinguishing the malformed case from the two declined ones is the point: only the first
/// says the file is broken. The reported geometry is asserted beside each class because the
/// line is **representability, not validity** — a geometry this crate can read is reported
/// even when what it reads is what declines the position.
#[test]
fn the_geometry_field_count_separates_the_malformed_case_from_the_two_declined_ones() {
    let stored = le_u16(&samples());
    let case = |geometry: &str| {
        first_header(attached_image(
            &format!(r#"geometry="{geometry}" sampleFormat="UInt16""#),
            stored.clone(),
        ))
    };

    // Fewer than two fields is not a geometry at all.
    let header = case("12");
    let decline = header.decline_reason().expect("declined");
    assert_eq!(decline.class(), DeclineClass::Malformed, "{decline:?}");
    assert_eq!(geometry_of(&header), (None, None, None));

    // Exactly two fields is a valid *one-dimensional* image: `dim_1:channel-count`.
    let header = case("12:1");
    let decline = header.decline_reason().expect("declined");
    assert_eq!(decline.class(), DeclineClass::Unsupported, "{decline:?}");
    assert_eq!(geometry_of(&header), (None, None, None));

    // Four fields is a valid three-dimensional image, and equally out of scope.
    let header = case("4:3:2:1");
    let decline = header.decline_reason().expect("declined");
    assert_eq!(decline.class(), DeclineClass::Unsupported, "{decline:?}");
    assert_eq!(geometry_of(&header), (None, None, None));

    // A zero-length axis reads as three fields, so it reports full geometry — and §8.5.1
    // calls it an empty image and forbids serializing one, so it is `Malformed` all the same.
    let header = case("4:0:1");
    let decline = header.decline_reason().expect("declined");
    assert_eq!(decline.class(), DeclineClass::Malformed, "{decline:?}");
    assert_eq!(geometry_of(&header), (Some(4), Some(0), Some(1)));

    // A negative field has no value to report through unsigned accessors, and structural
    // validity of the values is settled before scope, so this is `Malformed` rather than the
    // `Unsupported` a three-dimensional image gets.
    let header = case("4:-3:1");
    let decline = header.decline_reason().expect("declined");
    assert_eq!(decline.class(), DeclineClass::Malformed, "{decline:?}");
    assert_eq!(geometry_of(&header), (None, None, None));
}

/// Row *An `<Image>` with no `location` attribute is `Malformed`*, row *`inline` is not a legal
/// location for image pixel data*, and the `url(…)`/`path(…)` half of the format matrix.
#[test]
fn the_three_pixel_locations_this_version_refuses_are_all_malformed() {
    let stored = le_u16(&samples());

    // §10 requires a block's location *and role* to be completely defined by the header and
    // §11.5 requires an image's pixels to be a single data block, so this follows even though
    // §11.5.1's attribute list does not name `location`.
    let (class, reason) = declined(xml_unit(
        r#"<Image geometry="4:3:1" sampleFormat="UInt16"/>"#,
    ));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(reason.contains("location"), "{reason}");

    // §11.5 is explicit that an Image element cannot serialize pixel data as an inline block,
    // because an Image may have child elements. §7.2 lists inline among the locations a
    // baseline decoder reads, but that is about data blocks in general and §11.5 is the
    // specific rule — so the crate's §7.2 claim is qualified rather than asserted.
    let (class, reason) = declined(xml_unit(&format!(
        r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="inline:base64">{}</Image>"#,
        base64(&stored)
    )));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(reason.contains("inline"), "{reason}");

    // §10.2 forbids an external block in a monolithic unit outright, so these are the file
    // contradicting the format rather than a feature this version declines.
    for spelling in ["url(pixels.bin)", "path(/data/pixels.bin)"] {
        let (class, reason) = declined(located_u16(|_p, _s| spelling.to_owned(), &stored));
        assert_eq!(class, DeclineClass::Malformed, "{spelling}: {reason}");
    }
}

/// Row *A negative `offset` is `Malformed`* — the one place report-don't-interpret does not
/// extend to reporting.
#[test]
fn a_negative_or_non_finite_offset_is_malformed_and_a_legal_one_is_reported() {
    let levels = samples();
    let stored = le_u16(&levels);

    // §11.5.2 defines `offset` as a scalar whose value must be greater than or equal to zero,
    // and §8.3.3 makes `NaN` and `-Inf` expressible — so the attribute is outside the range
    // the specification defines for it rather than merely unusual.
    for spelling in ["-1", "-0.5", "NaN", "-Inf"] {
        let (class, reason) = declined(attached_u16(
            &format!(r#"offset="{spelling}""#),
            stored.clone(),
        ));
        assert_eq!(class, DeclineClass::Malformed, "{spelling}: {reason}");
    }

    // A legal one is reported and applied to nothing.
    let header = decodes_to(
        attached_u16(r#"offset="128.5""#, stored.clone()),
        &levels,
        "a declared offset changes no pixel",
    );
    assert_eq!(header.offset(), Some(128.5));
    // Absent reports §11.5.2's `0` default rather than a distinct "absent" state.
    let header = decodes_to(attached_u16("", stored), &levels, "no offset");
    assert_eq!(header.offset(), Some(0.0));
}

// ------------------------------------------------------------------ subblocks

/// Row *`subblocks` without `compression` is `Malformed`* and row *Three checks are added to
/// the subblock list*.
///
/// §10.6 requires no validation of the list and explicitly sets no upper limit on the number
/// of subblocks, so without these three the attribute is a cheap amplification vector the
/// element-count cap does not cover — the whole list being one attribute string rather than
/// elements.
#[test]
fn the_three_subblock_checks_hold_and_subblocks_without_compression_is_malformed() {
    let levels = samples();
    let plain = le_u16(&levels);
    let (stored, list) = subblocked(&plain, lz4);

    // The baseline: a well-formed split decodes to asserted pixels, so the three refusals
    // below are refusals of something and not of everything.
    let good = attached_u16(
        &format!(r#"compression="lz4:{}" subblocks="{list}""#, plain.len()),
        stored.clone(),
    );
    let header = decodes_to(good, &levels, "a well-formed subblock split");
    assert_eq!(header.granularity(), Granularity::Block { subblocks: 2 });

    // The attribute describes how compressed data was split; on an uncompressed block it
    // describes nothing. This crate's decision, not a spec rule.
    let (class, reason) = declined(attached_u16(
        &format!(r#"subblocks="{list}""#),
        plain.clone(),
    ));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(reason.contains("compression"), "{reason}");

    // Check 1 — the declared compressed lengths must sum to the stored block size.
    let (first, rest) = list.split_once(',').expect("a c,u pair");
    let wrong_compressed = format!("{},{rest}", first.parse::<u64>().unwrap() + 1);
    let (class, reason) = declined(attached_u16(
        &format!(
            r#"compression="lz4:{}" subblocks="{wrong_compressed}""#,
            plain.len()
        ),
        stored.clone(),
    ));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(reason.contains("compressed lengths sum"), "{reason}");

    // Check 2 — the declared uncompressed lengths must sum to the geometry-implied size.
    let (stored_short, list_short) = subblocked(&plain[..plain.len() - 2], lz4);
    let (class, reason) = declined(attached_u16(
        &format!(
            r#"compression="lz4:{}" subblocks="{list_short}""#,
            plain.len()
        ),
        stored_short,
    ));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(reason.contains("uncompressed lengths sum"), "{reason}");

    // Check 3 — the count is capped, and tripping a configured cap is `LimitExceeded` rather
    // than `Malformed`: the file is valid and self-consistent.
    let many: Vec<String> = (0..8).map(|_| "1,1".to_owned()).collect();
    let mut limits = astroframe::Limits::default();
    limits.subblock_count = 4;
    let bytes = attached_u16(
        &format!(
            r#"compression="lz4:{}" subblocks="{}""#,
            plain.len(),
            many.join(":")
        ),
        stored,
    );
    let mut reader = Reader::seekable_with_limits(Cursor::new(bytes), limits)
        .expect("the unit itself constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("a declined position still reports");
    let decline = header.decline_reason().expect("the position is declined");
    assert_eq!(decline.class(), DeclineClass::LimitExceeded, "{decline:?}");
    // All three run before any allocation: the count cap is enforced while the list is still
    // being parsed, so a list declaring 2^32 subblocks never becomes a Vec.
    assert!(decline.reason().contains("subblock count"), "{decline:?}");
}

// ------------------------------------------------------------------ item-size

/// Row *`item-size` comes from the compression attribute's mandatory third field and is never
/// derived from `sampleFormat`*.
///
/// `0` is rejected, `1` is a valid no-op, a value exceeding the block length is `Malformed`,
/// and a trailing partial item is copied through unshuffled.
#[test]
fn item_size_one_is_a_no_op_and_a_trailing_partial_item_is_copied_through() {
    let levels = samples();
    let plain = le_u16(&levels);

    // `item-size == 1`: one byte per item is one plane, and the transform is the identity —
    // so the shuffled bytes are the plain ones and the decode must still be exact.
    let shuffled = shuffle(&plain, 1);
    assert_eq!(
        shuffled, plain,
        "the fixture's own transform is the identity"
    );
    let unit = attached_u16(
        &format!(r#"compression="zlib+sh:{}:1""#, plain.len()),
        zlib(&shuffled),
    );
    decodes_to(unit, &levels, "item-size 1 is a no-op");

    // The trailing partial item, in the specification-conforming shape § XISF decisions names:
    // a three-sample `UInt16` block with a legal `item-size="4"` is six bytes with two left
    // over. The planes are subsets of *equally significant bytes*, which exist only for
    // complete items, so the two spare bytes belong to no plane and pass through as stored.
    let three = [0u16, 257, 65535];
    let plain3 = le_u16(&three);
    assert_eq!(plain3.len() % 4, 2, "the fixture leaves a partial item");
    let unit = attached_image(
        &format!(
            r#"geometry="3:1:1" sampleFormat="UInt16" compression="zlib+sh:{}:4""#,
            plain3.len()
        ),
        zlib(&shuffle(&plain3, 4)),
    );
    let (_, got) = read_one(unit);
    assert_same_bits(&got, &expected_u16(&three), "a trailing partial item");

    // `0` describes no transform at all.
    let (class, reason) = declined(attached_u16(
        &format!(r#"compression="zlib+sh:{}:0""#, plain.len()),
        zlib(&plain),
    ));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");

    // A value exceeding the block length describes a transform with no complete item in it,
    // and the length comes from the geometry rather than from the attribute — which is why
    // the check cannot live in the attribute parser.
    let (class, reason) = declined(attached_u16(
        &format!(r#"compression="zlib+sh:{}:99""#, plain.len()),
        zlib(&plain),
    ));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");

    // A `+sh` codec missing the field is `Malformed` rather than a case for inference: §10.6
    // defines `item-size` only as "the length in bytes of a data item" and never ties it to
    // the sample width, so there is nothing to infer it from.
    let (class, reason) = declined(attached_u16(
        &format!(r#"compression="zlib+sh:{}""#, plain.len()),
        zlib(&plain),
    ));
    assert_eq!(class, DeclineClass::Malformed, "{reason}");
    assert!(reason.contains("fields"), "{reason}");
}

// ------------------------------------------------------------------ checksums

/// Row *Checksums are verified for every block whose contents are actually read* and row
/// *All five algorithms are supported, not the mandatory one alone*.
///
/// §10.5 makes SHA-1 mandatory for a decoder claiming checksum support and the other four
/// optional, so a cheaper sha1-only build would be conformant — which is exactly why the four
/// need a test.
#[test]
fn every_checksum_algorithm_verifies_an_attached_block() {
    let levels = samples();
    let stored = le_u16(&levels);
    // Both spellings of each name, since §10.5 Table 9 gives every algorithm two.
    for algorithm in [
        "sha-1", "sha1", "sha-256", "sha256", "sha-512", "sha512", "sha3-256", "sha3-512",
    ] {
        let unit = attached_u16(&checksum_attr(algorithm, &stored), stored.clone());
        let header = decodes_to(unit, &levels, algorithm);
        // The digest covers the whole stored block, so nothing may be delivered until all of
        // it has been read and hashed — the checksum floor, ignoring any subblock split.
        assert_eq!(header.granularity(), Granularity::WholeImage, "{algorithm}");
    }
}

/// The other half of the same row: an `attachment` block's contents are **not** read at tier 1,
/// so its digest is verified at the pixel call rather than at construction.
///
/// The `embedded` counterpart — verified at construction, because its bytes live in the header
/// region — is the decline table's own row and is graded in `xisf_declines.rs`.
#[test]
fn a_mismatched_attachment_digest_surfaces_at_the_pixel_call_not_at_construction() {
    let levels = samples();
    let stored = le_u16(&levels);
    // A digest over *different* bytes: legal syntax, wrong value.
    let wrong = checksum_attr("sha-1", &le_u16(&[7u16; 12]));
    let mut reader = seekable(attached_u16(&wrong, stored)).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("an advanced reader has a header");
    // Tier 1 stays free for an attachment: nothing was read, so nothing was verified, and the
    // position is not declined.
    assert!(header.decline_reason().is_none(), "tier 1 reads no block");
    let err = reader
        .read_image_into(&mut [0.0; 12])
        .expect_err("the digest does not match");
    assert_eq!(kind(&err), "ChecksumMismatch", "{err}");
}

// ------------------------------------------------------------------ thumbnails
/// **The stored block is measured against `Materialized bytes` before it is read**, not merely
/// narrowed.
///
/// It is the largest buffer a `WholeImage` decode allocates and it is sized from a *declared*
/// length, so § The caps' "every buffer this crate allocates for itself" has to cover it. Two
/// existing checks look like they already do and do not: the geometry cross-check bounds an
/// *uncompressed* block's stored size, and the `implied_bytes` check bounds the *decompressed*
/// buffer. Neither bounds a **compressed** block's stored bytes, which the stored-block cap
/// alone governs — and that cap's 1 MiB floor applies whatever the geometry. So a hundred-pixel
/// image could pull a megabyte into memory under a twenty-thousand-byte materialization cap.
#[test]
fn a_compressed_stored_block_is_measured_against_the_materialization_cap() {
    let levels = samples();
    let plain = le_u16(&levels);
    // A bare LZ4 block with no `subblocks` never streams, so this is the materializing path.
    // The stored bytes are padded far past what the tiny geometry implies, which the
    // stored-block cap permits: its floor is a mebibyte however small the image.
    let mut stored = lz4(&plain);
    stored.resize(600_000, 0u8);
    let unit = Unit::new()
        .attached(
            &format!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" compression="lz4:{}" {{loc}}/>"#,
                plain.len()
            ),
            stored,
        )
        .build();

    let mut tight = astroframe::Limits::default();
    tight.materialized_bytes = 20_000;
    let mut reader =
        Reader::seekable_with_limits(Cursor::new(unit), tight).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let err = reader
        .read_image()
        .expect_err("600 kB of stored bytes is above a 20 kB materialization cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("materialized bytes"), "{err}");

    // The control: the same shape with a stored block the cap admits decodes to its pixels, so
    // the refusal above is of the size and not of the codec or the path. The padding is dropped
    // here because trailing bytes corrupt the LZ4 stream itself, which would refuse the control
    // for the wrong reason.
    let honest = Unit::new()
        .attached(
            &format!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" compression="lz4:{}" {{loc}}/>"#,
                plain.len()
            ),
            lz4(&plain),
        )
        .build();
    let mut reader =
        Reader::seekable_with_limits(Cursor::new(honest), tight).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let image = reader.read_image().expect("a stored block under the cap");
    assert_same_bits(
        &image.into_samples(),
        &expected_u16(&levels),
        "a stored block under the cap",
    );
}

/// **A header-only prefix of a unit that carries a `<Thumbnail>` still parses.**
///
/// This is criterion 15's shape for the file every real XISF is: § Local corpus validation
/// records that *every* variant in the 1080-file corpus carries a `Thumbnail`, so a consumer
/// fetching a size-capped prefix of a remote frame hands this crate exactly this.
///
/// It is a regression test with a story. § Hardening's corollary — **a declared block offset
/// is never validated during the header phase** — was briefly broken by a construction-time
/// scan that checked every `Thumbnail`'s declared attachment against the source length, added
/// to implement the `Malformed` half of § XISF decisions' `Thumbnail` row. It made all 1080 of
/// those files `Malformed` as prefixes. The existing criterion-15 test did not catch it
/// because its fixture has no `Thumbnail`, which is the one shape a real file never has.
#[test]
fn a_header_only_prefix_of_a_unit_carrying_a_thumbnail_still_parses() {
    let levels = samples();
    let unit = Unit::new()
        .attached(
            r#"<Thumbnail geometry="2:2:1" sampleFormat="UInt8" {loc}/>"#,
            vec![0u8; 4],
        )
        .attached(&image_element(""), le_u16(&levels))
        .build();

    // Everything after the declared header region is dropped, which is what a size-capped
    // fetch of a remote file produces: both declared attachments now point past the end.
    let declared = u32::from_le_bytes(unit[8..12].try_into().expect("the preamble")) as usize;
    let prefix = unit[..16 + declared].to_vec();
    assert!(prefix.len() < unit.len(), "the prefix must really be short");

    let mut reader = seekable(prefix).expect("a header-only prefix parses");
    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("geometry is reported");
    assert_eq!((header.width(), header.height()), (Some(4), Some(3)));
    // And the mismatch is an error only when someone asks for the pixels that are not there.
    let err = reader
        .read_image()
        .expect_err("the pixels are past the end of the prefix");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// Row *`Thumbnail` elements are skipped and their data blocks stepped over*, bounded by the
/// **`Skipped block bytes`** cap rather than by the stored-block cap.
///
/// A thumbnail is not an image this crate reports, so `next_image()` never yields one — but a
/// sequential source still has to step over its attached block to reach what follows.
#[test]
fn a_thumbnails_attached_block_is_stepped_over_and_the_right_cap_bounds_the_step() {
    let levels = samples();
    // Two mebibytes, which is well above the stored-block cap for this image: that cap is
    // `max(implied × 2, 1 MiB)` and the image implies 24 bytes. If the thumbnail's block were
    // measured by the *current image's* geometry — the wrong instrument, since a thumbnail
    // has its own geometry and may sit at the root with no current image at all — this would
    // fail, and it is also the size that would make an allocation from the declared length
    // visible.
    let thumbnail_bytes = vec![0xa5u8; 2 << 20];
    let unit = Unit::new()
        .attached(
            r#"<Thumbnail geometry="1024:1024:2" sampleFormat="UInt8" {loc}/>"#,
            thumbnail_bytes,
        )
        .attached(&image_element(""), le_u16(&levels));

    // A seekable source steps over it with a seek and never consults the cap at all.
    let (_, got) = read_one(unit.build());
    assert_same_bits(&got, &expected_u16(&levels), "a thumbnail before the image");

    // A sequential source must read in order to skip, and the read is bounded by
    // `Skipped block bytes` — without which a declared 2⁶³-byte thumbnail on a pipe is an
    // unbounded read.
    let mut reader = sequential(unit.build()).expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let image = reader.read_image().expect("the image decodes");
    assert_same_bits(
        &image.into_samples(),
        &expected_u16(&levels),
        "sequential: the thumbnail's block is stepped over",
    );

    // Tighten that cap and the same source refuses the step — `LimitExceeded`, the file being
    // valid and self-consistent and having tripped a configured cap.
    let mut limits = astroframe::Limits::default();
    limits.skipped_block_bytes = 1024;
    let mut reader = Reader::sequential_with_limits(Cursor::new(unit.build()), limits)
        .expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let err = reader
        .read_image_into(&mut [0.0; 12])
        .expect_err("the step over the thumbnail exceeds the cap");
    assert_eq!(kind(&err), "LimitExceeded", "{err}");
    assert!(format!("{err}").contains("skipped block bytes"), "{err}");

    // The same tight cap on a **seekable** source decodes: skipping is a seek there, the
    // cursor moves without transferring bytes, and the cap is not consulted.
    let mut reader = Reader::seekable_with_limits(Cursor::new(unit.build()), limits)
        .expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let image = reader.read_image().expect("a seek costs no skipped bytes");
    assert_same_bits(
        &image.into_samples(),
        &expected_u16(&levels),
        "seekable: skipping is a seek",
    );
}

// ------------------------------------------- the elements this crate meets and does not read

/// Row *The core elements this crate meets and does not read are dispositioned explicitly*,
/// and the silent half of row *"Declined" means two different things*.
///
/// An element never fails a frame it does not prevent decoding. `RGBWorkingSpace` appears in
/// §11.13's own worked example and PixInsight writes it routinely, so treating it as
/// frame-level would refuse a large share of real RGB files for a colour-management element
/// that has nothing to do with pixels.
#[test]
fn the_declined_elements_never_fail_the_frame_and_the_two_reported_ones_are_reachable() {
    let levels = samples();
    let children = concat!(
        r#"<ICCProfile location="attachment:99999:16"/>"#,
        r#"<RGBWorkingSpace x="0.6:0.3:0.1" Y="0.2:0.7:0.1" gamma="2.2"/>"#,
        r#"<Table><Property id="Table:Col" type="String" value="skipped"/></Table>"#,
        r#"<Structure location="attachment:99999:16"/>"#,
        r#"<Resolution horizontal="300" vertical="150" unit="cm"/>"#,
        r#"<DisplayFunction m="0.25:0.25:0.25:0.25" s="0.1:0.1:0.1:0.1"/>"#,
        // An element no version of the specification defines: ignored, which is the only
        // reading under which a 1.0 decoder survives a later revision.
        r#"<SomethingFromTheFuture answer="42"/>"#,
    );
    let header = decodes_to(
        attached_u16_with("", children, le_u16(&levels)),
        &levels,
        "declined elements never fail the frame",
    );

    // `Resolution` and `DisplayFunction` are *reported* — metadata the file states and no
    // consumer can recover otherwise.
    let resolution = header.resolution().expect("XISF defines a resolution");
    assert_eq!(resolution.horizontal(), 300.0);
    assert_eq!(resolution.vertical(), 150.0);
    assert_eq!(resolution.unit(), astroframe::ResolutionUnit::Centimetre);
    let df = header
        .display_function()
        .expect("XISF defines a display function");
    assert_eq!(df.midtones().red_gray, 0.25);
    assert_eq!(df.shadows().lightness, 0.1);
    // An attribute the element did not carry keeps the identity function's value.
    assert_eq!(df.highlights().blue, 1.0);

    // The `Table`'s property is declined with it, rather than leaking into the image's list.
    assert!(header.properties().is_empty(), "{:?}", header.properties());
    // `ICCProfile` carries a data block whose declared position is nonsense; nothing reads it,
    // which is what "declined" means for an element.
    assert!(header.cfa().is_none(), "absence means not mosaiced");

    // Both reported elements have specification-defined defaults, and absence reports them.
    let header = decodes_to(
        attached_u16("", le_u16(&levels)),
        &levels,
        "the ancillary defaults",
    );
    let resolution = header.resolution().expect("the XISF default");
    assert_eq!(resolution.horizontal(), 72.0);
    assert_eq!(resolution.vertical(), 72.0);
    assert_eq!(resolution.unit(), astroframe::ResolutionUnit::Inch);
    let df = header.display_function().expect("the XISF default");
    assert_eq!(df.midtones().red_gray, 0.5);
    assert_eq!(df.shadows().red_gray, 0.0);
    assert_eq!(df.highlights().red_gray, 1.0);
}

// ------------------------------------------------------------- defaults and channel counts

/// Row *`pixelStorage` absent means `Planar`, `colorSpace` absent means `Gray` — never
/// inferred from channel count. Channel count is never validated against the colour space*.
///
/// The second half is the load-bearing one: a decoder that defaults correctly and then checks
/// "`Gray` implies one channel" rejects three legal combinations.
#[test]
fn the_defaults_are_never_inferred_and_the_channel_count_is_never_validated() {
    // Absent means `Gray` and `Planar` even on a three-channel image.
    let three = repeating_u16(12);
    let header = decodes_to(
        attached_image(r#"geometry="2:2:3" sampleFormat="UInt16""#, le_u16(&three)),
        &three,
        "three channels with no colorSpace",
    );
    assert_eq!(header.color_space(), Some(ColorSpace::Gray));
    assert_eq!(header.pixel_storage(), Some(PixelStorage::Planar));
    assert_eq!(header.channels(), Some(3));

    // And the converse: `RGB` with a fourth channel. §8.5.1 calls channels beyond the nominal
    // count alpha channels; this crate has no visual role and delivers them as ordinary
    // channels.
    let four = repeating_u16(16);
    let header = decodes_to(
        attached_image(
            r#"geometry="2:2:4" sampleFormat="UInt16" colorSpace="RGB""#,
            le_u16(&four),
        ),
        &four,
        "an RGB image with an alpha channel",
    );
    assert_eq!(header.color_space(), Some(ColorSpace::Rgb));
    assert_eq!(header.channels(), Some(4));

    // And `RGB` with a single channel, the third of the three combinations a channel-count
    // check would reject.
    let one = repeating_u16(4);
    let header = decodes_to(
        attached_image(
            r#"geometry="2:2:1" sampleFormat="UInt16" colorSpace="RGB""#,
            le_u16(&one),
        ),
        &one,
        "a single-channel RGB image",
    );
    assert_eq!(header.channels(), Some(1));
}

/// Row *`FITSKeyword` must be a child of an `Image` or of the root* and row *Property
/// identifiers are reported verbatim and never validated as tokens*.
#[test]
fn a_keyword_inside_metadata_is_ignored_and_a_property_id_is_never_validated() {
    let levels = samples();
    let unit = Unit::new()
        .attached(
            &format!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" {{loc}}>{}</Image>"#,
                r#"<Property id="Instrument: colorFlag" type="Boolean" value="true"/>"#
            ),
            le_u16(&levels),
        )
        // §11.6 places `FITSKeyword` under an `Image` or the root. One inside `Metadata` is a
        // non-conforming placement attached to no image, and reporting it against an
        // arbitrary one would invent an association the file does not make.
        .xml(r#"<Metadata><FITSKeyword name="OBJECT" value="'M31'" comment="c"/></Metadata>"#)
        .build();
    let header = decodes_to(unit, &levels, "a keyword inside Metadata");
    assert!(header.keywords().is_empty(), "{:?}", header.keywords());
    assert!(header.get("OBJECT").is_none());

    // A space-bearing identifier has been reported in the wild, and validating ids against a
    // well-formed `Namespace:Path` grammar would reject the file that carries one.
    assert_eq!(
        property(&header, "Instrument: colorFlag").property_type(),
        &PropertyType::Boolean
    );
}

// ------------------------------------------------- *Reported metadata is reachable*

/// Criterion *Reported metadata is reachable*.
///
/// Most of the XISF facts are XML **attributes** — neither `FITSKeyword` nor `Property`
/// elements — so no keyword or property lookup reaches them. This is what makes
/// report-don't-interpret checkable rather than aspirational.
#[test]
fn every_reported_attribute_is_reachable_by_its_own_accessor() {
    let levels = repeating_u16(12);
    let header = decodes_to(
        attached_image(
            concat!(
                r#"geometry="2:2:3" sampleFormat="UInt16" colorSpace="RGB" "#,
                r#"pixelStorage="Planar" orientation="180" offset="12.5" "#,
                r#"bounds="0:65535" id="light_0042" "#,
                r#"uuid="6d2f0b48-0000-4000-8000-000000000001" imageType="MasterLight""#
            ),
            le_u16(&levels),
        ),
        &levels,
        "every reported attribute",
    );

    assert_eq!(header.orientation(), Some(&Orientation::Rotate180));
    assert_eq!(header.offset(), Some(12.5));
    // A redundant `bounds` still reports `Declared`, not `FormatDefault`: §11.5.1 only says
    // such a `bounds` *should not* be written, so real writers produce it — and a consumer's
    // envelope predicate has to be able to see the difference.
    assert_eq!(*header.bounds(), astroframe::Bounds::Declared(0.0, 65535.0));
    assert_eq!(header.color_space(), Some(ColorSpace::Rgb));
    assert_eq!(header.pixel_storage(), Some(PixelStorage::Planar));
    assert_eq!(header.image_id(), Some("light_0042"));
    assert_eq!(
        header.image_uuid(),
        Some("6d2f0b48-0000-4000-8000-000000000001")
    );
    assert_eq!(header.image_type(), Some(&ImageType::MasterLight));
    // `scaling()` is `None` for XISF and `row_order()` has no XISF meaning: an accessor whose
    // format does not define the concept reports `None` rather than a fabricated value.
    assert_eq!(header.scaling(), None);
    assert_eq!(header.row_order(), None);

    // None of them is reachable through the two text surfaces, which is the whole reason the
    // accessors exist.
    assert!(header.get("orientation").is_none());
    assert!(header.get("imageType").is_none());
    assert!(header.properties().is_empty());

    // `imageType` and `orientation` are closed enumerations, but decoding does not depend on
    // them, so an unrecognized value degrades to "unknown" and is reported as text.
    let header = decodes_to(
        attached_u16(
            r#"orientation="37;flip" imageType="MasterSuperLight""#,
            le_u16(&levels),
        ),
        &levels,
        "unknown closed values",
    );
    assert_eq!(
        header.orientation(),
        Some(&Orientation::Other("37;flip".into()))
    );
    assert_eq!(
        header.image_type(),
        Some(&ImageType::Other("MasterSuperLight".into()))
    );

    // An absent `orientation` reports `Identity` rather than a distinct "absent" state: `0`
    // and absence mean the same thing for a spec attribute with a defined default.
    let header = decodes_to(attached_u16("", le_u16(&levels)), &levels, "absent");
    assert_eq!(header.orientation(), Some(&Orientation::Identity));
    assert_eq!(header.image_id(), None);
    assert_eq!(header.image_uuid(), None);
    assert_eq!(header.image_type(), None);
    // An integer image with no `bounds` takes §8.5.5's [0, 2ⁿ − 1] default, and `bounds()`
    // carries the pair so a tier-3 caller normalizing chunks reads the range directly instead
    // of re-deriving it from the sample width.
    assert_eq!(
        *header.bounds(),
        astroframe::Bounds::FormatDefault(0.0, 65535.0)
    );
}

// --------------------------------- *Metadata that has no FITS equivalent survives*

/// Criterion *Metadata that has no FITS equivalent survives, in all its shapes*.
///
/// A `String` property has no `value` attribute by specification, so testing only the
/// attribute-borne shape would pass while dropping every `Observation:Object:Name` in
/// existence.
#[test]
fn property_values_survive_in_all_three_of_their_shapes() {
    let levels = samples();
    let children = concat!(
        // Shape 1 — an attribute-valued property, with both optional attributes present.
        r#"<Property id="Instrument:ExposureTime" type="Float32" value="120.5" "#,
        r#"format="%.2f" comment="seconds"/>"#,
        // Shape 2 — a character-data `String` property. §11.1.6 forbids it a `value`.
        r#"<Property id="Observation:Object:Name" type="String">NGC 7000</Property>"#,
        // Shape 3 — a block-valued property. Reported `Unavailable`, never dropped: a
        // consumer must be able to tell "the file does not carry this" from "it does and this
        // version cannot read it".
        r#"<Property id="Processing:AstrometricSolution" type="F64Matrix" "#,
        r#"location="attachment:100000:64"/>"#,
        // The three `type` cases: a primary name, an alternate spelling, and one this version
        // does not recognize.
        r#"<Property id="Test:Primary" type="TimePoint" value="2031-04-05T22:10:00Z"/>"#,
        r#"<Property id="Test:AlternateScalar" type="Byte" value="7"/>"#,
        r#"<Property id="Test:AlternateVector" type="Vector" location="attachment:100000:8"/>"#,
        r#"<Property id="Test:Unknown" type="Table" value="excluded by §11.1"/>"#,
        // Duplicate `HISTORY` keywords, all exposed, in document order.
        r#"<FITSKeyword name="HISTORY" value="" comment="first step"/>"#,
        r#"<FITSKeyword name="HISTORY" value="" comment="second step"/>"#,
        r#"<FITSKeyword name="HISTORY" value="" comment="third step"/>"#,
    );
    let header = decodes_to(
        attached_u16_with("", children, le_u16(&levels)),
        &levels,
        "property shapes",
    );

    let exposure = property(&header, "Instrument:ExposureTime");
    assert_eq!(exposure.property_type(), &PropertyType::Float32);
    // Verbatim text, never parsed per the declared type: re-rendering a number through a
    // formatter can lose digits, and the consumer is the one that parses.
    assert_eq!(text_of(exposure), "120.5");
    assert_eq!(exposure.format(), Some("%.2f"));
    assert_eq!(exposure.comment(), Some("seconds"));
    assert_eq!(exposure.scope(), PropertyScope::Image);

    let object = property(&header, "Observation:Object:Name");
    assert_eq!(object.property_type(), &PropertyType::String);
    assert_eq!(text_of(object), "NGC 7000");
    assert_eq!(object.format(), None);
    assert_eq!(object.comment(), None);

    let solution = property(&header, "Processing:AstrometricSolution");
    assert_eq!(solution.value(), &PropertyValue::Unavailable);
    // Carrying its type is what lets a consumer tell a missing astrometric solution from one
    // this version cannot read.
    assert_eq!(solution.property_type(), &PropertyType::F64Matrix);

    // The three `type` cases, which is what "graded on all three" means.
    assert_eq!(
        property(&header, "Test:Primary").property_type(),
        &PropertyType::TimePoint
    );
    // `Byte` is Table 3's alternate spelling of `UInt8` and `Vector` is Table 7's of
    // `F64Vector`; both name one type, so both resolve to the primary's variant rather than
    // falling into the catch-all.
    assert_eq!(
        property(&header, "Test:AlternateScalar").property_type(),
        &PropertyType::UInt8
    );
    assert_eq!(
        property(&header, "Test:AlternateVector").property_type(),
        &PropertyType::F64Vector
    );
    // §11.1 excludes table properties from `Property` altogether, so `Table` is
    // non-conforming and lands in `Other` like any other unrecognized name — the reporting
    // answer rather than the fatal one.
    assert_eq!(
        property(&header, "Test:Unknown").property_type(),
        &PropertyType::Other("Table".into())
    );

    // Every duplicate `HISTORY`, in document order. `HISTORY` carries an empty value by
    // specification and its text lives in the comment.
    let history: Vec<&str> = header
        .keywords()
        .iter()
        .filter(|k| k.name() == "HISTORY")
        .map(|k| k.comment().expect("XISF makes comment mandatory"))
        .collect();
    assert_eq!(history, vec!["first step", "second step", "third step"]);
    assert_eq!(header.get("HISTORY").map(|k| k.value()), Some(""));
}

// ------------------------------------------- *`granularity()` reports the right value*

/// Criterion *`granularity()` reports the right value* — one fixture per row of § Streaming's
/// table, asserting the **reported** value and decoding each to asserted pixels.
///
/// Each property of a block imposes a floor and the granularity is the **worst** of them, not
/// the first one found. The combinations a first-match implementation gets wrong are the
/// point, and they are the four in the middle.
#[test]
fn granularity_is_the_worst_floor_not_the_first() {
    let levels = samples();
    let plain = le_u16(&levels);
    let size = plain.len();
    let shuffled = shuffle(&plain, 2);

    let (lz4_split, lz4_list) = subblocked(&plain, lz4);
    let (zlib_split, zlib_list) = subblocked(&plain, zlib);
    // The shuffle spans the whole **pre-split** block, so the split is taken over the
    // shuffled bytes — which is exactly why the subblock boundaries buy nothing.
    let (shuffled_split, shuffled_list) = subblocked(&shuffled, lz4);

    let checksummed_split = checksum_attr("sha-1", &lz4_split);
    let checksummed_shuffled_split = checksum_attr("sha-256", &shuffled_split);
    let checksummed_plain = checksum_attr("sha-512", &plain);

    // Every row's pixels are asserted here as well as its reported floor, which is what makes
    // the reported value describe a decode that happens rather than one that does not.
    let rows: Vec<(&str, Vec<u8>, Granularity)> = vec![
        (
            "uncompressed, no checksum",
            attached_u16("", plain.clone()),
            Granularity::Rows,
        ),
        (
            "zlib, no shuffling, no checksum",
            attached_u16(&format!(r#"compression="zlib:{size}""#), zlib(&plain)),
            Granularity::Rows,
        ),
        (
            // `Block` is reachable *only* this way: zlib and zstd already stream by rows, and
            // shuffling or a checksum forces `WholeImage`.
            "lz4 + subblocks, no shuffling, no checksum",
            attached_u16(
                &format!(r#"compression="lz4:{size}" subblocks="{lz4_list}""#),
                lz4_split.clone(),
            ),
            Granularity::Block { subblocks: 2 },
        ),
        (
            // `subblocks` only blocks a promotion; it never lowers a `Rows` floor.
            "zlib + subblocks, no shuffling, no checksum",
            attached_u16(
                &format!(r#"compression="zlib:{size}" subblocks="{zlib_list}""#),
                zlib_split,
            ),
            Granularity::Rows,
        ),
        (
            "subblocks + shuffling",
            attached_u16(
                &format!(r#"compression="lz4+sh:{size}:2" subblocks="{shuffled_list}""#),
                shuffled_split.clone(),
            ),
            Granularity::WholeImage,
        ),
        (
            // The digest covers the whole **stored** block (§10.5), which `subblocks` does not
            // split, so nothing may be delivered until all of it is read and hashed.
            "subblocks + checksum, no shuffling",
            attached_u16(
                &format!(r#"compression="lz4:{size}" subblocks="{lz4_list}" {checksummed_split}"#),
                lz4_split.clone(),
            ),
            Granularity::WholeImage,
        ),
        (
            "checksummed + shuffled + subblocked",
            attached_u16(
                &format!(
                    r#"compression="lz4+sh:{size}:2" subblocks="{shuffled_list}" {checksummed_shuffled_split}"#
                ),
                shuffled_split,
            ),
            Granularity::WholeImage,
        ),
        (
            // The pixels were fully materialized during the header parse, so no part of the
            // input remains to stream.
            "embedded",
            embedded_u16("", r#"encoding="base64""#, &base64(&plain)),
            Granularity::WholeImage,
        ),
        (
            "one lz4 block covering the image",
            attached_u16(&format!(r#"compression="lz4:{size}""#), lz4(&plain)),
            Granularity::WholeImage,
        ),
        (
            "one checksummed block covering the image",
            attached_u16(&checksummed_plain, plain.clone()),
            Granularity::WholeImage,
        ),
    ];

    for (what, bytes, want) in rows {
        // Reported *before* the decode, which is the whole point of the accessor — a caller
        // decides how to buffer from it rather than discovering the answer afterwards.
        let header = first_header(bytes.clone());
        assert_eq!(header.granularity(), want, "{what}");
        decodes_to(bytes, &levels, what);
    }
}

/// The row of § Streaming's table that is easiest to deliver wrongly: a subblocked `zlib`
/// block reports `Rows`, and the delivery path has to honour it.
///
/// §10.6 makes each subblock an **independently compressed** stream, so the stored bytes are N
/// concatenated zlib streams rather than one. A streaming path that opens a single stream over
/// the whole stored range ends after the first subblock's uncompressed bytes and reports the
/// block as truncated; the path restarts the codec at each boundary, driven by the declared
/// `(compressed, uncompressed)` pairs. Materializing the block whole would decode the same
/// pixels and contradict the `Rows` granularity the same header reports, which is why this
/// asserts the pixels of a *multi-subblock* fixture rather than only that a decode succeeds.
#[test]
fn a_subblocked_zlib_block_streams_by_rows() {
    let levels = samples();
    let plain = le_u16(&levels);
    let (stored, list) = subblocked(&plain, zlib);
    let unit = attached_u16(
        &format!(r#"compression="zlib:{}" subblocks="{list}""#, plain.len()),
        stored,
    );
    decodes_to(unit, &levels, "zlib + subblocks");
}

// ----------------------------------- *Baseline XISF decoder conformance* (XISF §7.2)

/// §7.2's *every standard compression codec* bullet, plus the `zstd` this crate adds.
///
/// The three container shapes are the point: LZ4 and zstd fail in **opposite** directions, so
/// a decoder reaching for a framed LZ4 reader breaks LZ4 and one reaching for a bare-block
/// zstd reader breaks zstd. Every row here decodes to asserted pixels.
#[test]
fn baseline_conformance_reads_every_standard_codec_and_its_shuffled_variant() {
    let levels = samples();
    let plain = le_u16(&levels);
    let size = plain.len();
    let shuffled = shuffle(&plain, 2);

    let cases: Vec<(String, Vec<u8>)> = vec![
        (format!(r#"compression="zlib:{size}""#), zlib(&plain)),
        (
            format!(r#"compression="zlib+sh:{size}:2""#),
            zlib(&shuffled),
        ),
        (format!(r#"compression="lz4:{size}""#), lz4(&plain)),
        (format!(r#"compression="lz4+sh:{size}:2""#), lz4(&shuffled)),
        // `lz4hc` is the same bare-block container written by a higher-effort compressor, so
        // an ordinary LZ4 block is a conforming `lz4hc` block.
        (format!(r#"compression="lz4hc:{size}""#), lz4(&plain)),
        (
            format!(r#"compression="lz4hc+sh:{size}:2""#),
            lz4(&shuffled),
        ),
        (format!(r#"compression="zstd:{size}""#), zstd_raw(&plain)),
        (
            format!(r#"compression="zstd+sh:{size}:2""#),
            zstd_raw(&shuffled),
        ),
    ];

    for (attribute, stored) in cases {
        decodes_to(attached_u16(&attribute, stored), &levels, &attribute);
    }
}

/// §7.2's *pixel data from embedded and attachment locations* bullet — the partial one, since
/// §11.5 forbids an `Image` from serializing pixel data inline at all.
///
/// Both locations over the same samples, so the two paths are proven to agree rather than
/// separately plausible.
#[test]
fn baseline_conformance_reads_both_pixel_locations_that_an_image_may_use() {
    let levels = samples();
    let stored = le_u16(&levels);
    let (_, from_attachment) = read_one(attached_u16("", stored.clone()));
    let (_, from_embedded) = read_one(embedded_u16("", r#"encoding="base64""#, &base64(&stored)));
    assert_same_bits(&from_attachment, &expected_u16(&levels), "attachment");
    assert_same_bits(&from_embedded, &from_attachment, "the two locations agree");
}

/// §7.2's *`Planar` and `Normal` pixel storage* bullet, and its *`Gray` and `RGB` colour
/// spaces* bullet.
///
/// The interleaved path is a transposition, and a transposition is where a decoder silently
/// corrupts: the two fixtures store the same image in the two layouts and must produce the
/// same planar output.
#[test]
fn baseline_conformance_reads_planar_and_normal_storage_in_gray_and_rgb() {
    const W: usize = 2;
    const H: usize = 2;
    const C: usize = 3;
    // A distinct level per (channel, row, column), so a transposition error cannot cancel.
    let level = |c: usize, r: usize, x: usize| (1000 * c + 10 * r + x) as u16;

    let mut planar = Vec::new();
    for c in 0..C {
        for r in 0..H {
            for x in 0..W {
                planar.push(level(c, r, x));
            }
        }
    }
    let mut interleaved = Vec::new();
    for r in 0..H {
        for x in 0..W {
            for c in 0..C {
                interleaved.push(level(c, r, x));
            }
        }
    }

    // `Planar` is the default and is written explicitly here, since the fixture's point is the
    // pair rather than the default.
    let from_planar = attached_image(
        r#"geometry="2:2:3" sampleFormat="UInt16" colorSpace="RGB" pixelStorage="Planar""#,
        le_u16(&planar),
    );
    let from_normal = attached_image(
        r#"geometry="2:2:3" sampleFormat="UInt16" colorSpace="RGB" pixelStorage="Normal""#,
        le_u16(&interleaved),
    );

    let (planar_header, planar_samples) = read_one(from_planar);
    let (normal_header, normal_samples) = read_one(from_normal);
    // The decode target is the whole image, **planar**, whatever the file's storage: the
    // output layout is the crate's contract and the input layout is the file's business.
    assert_same_bits(&planar_samples, &expected_u16(&planar), "Planar storage");
    assert_same_bits(&normal_samples, &planar_samples, "Normal storage");
    assert_eq!(planar_header.pixel_storage(), Some(PixelStorage::Planar));
    assert_eq!(normal_header.pixel_storage(), Some(PixelStorage::Normal));
    // Interleaving changes no granularity: every input row yields samples for all channels,
    // so the decoder never has to hold more of the *input*.
    assert_eq!(normal_header.granularity(), Granularity::Rows);

    // The `Gray` half of the colour-space bullet, over the same machinery.
    let gray = repeating_u16(4);
    let header = decodes_to(
        attached_image(
            r#"geometry="2:2:1" sampleFormat="UInt16" colorSpace="Gray""#,
            le_u16(&gray),
        ),
        &gray,
        "Gray",
    );
    assert_eq!(header.color_space(), Some(ColorSpace::Gray));
}

/// §7.2's *`UInt8`, `UInt16` and `Float32` sample formats* bullet, graded at both layers: the
/// normalized `f32` output and the native samples underneath it.
#[test]
fn baseline_conformance_reads_uint8_uint16_and_float32_samples() {
    // `UInt8`.
    let u8_levels: [u8; 12] = [0, 1, 2, 3, 127, 128, 129, 200, 253, 254, 255, 42];
    let (header, got) = read_one(attached_image(
        r#"geometry="4:3:1" sampleFormat="UInt8""#,
        le_u8(&u8_levels),
    ));
    assert_eq!(header.sample_format(), Some(SampleFormat::U8));
    assert_same_bits(&got, &expected_u8(&u8_levels), "UInt8");

    // `UInt16`.
    let u16_levels = samples();
    let header = decodes_to(attached_u16("", le_u16(&u16_levels)), &u16_levels, "UInt16");
    assert_eq!(header.sample_format(), Some(SampleFormat::U16));

    // `Float32`. §11.5.1 makes `bounds` mandatory for a floating point real image, so the
    // fixture declares one; the values straddle it so the saturating clamp is exercised too.
    let f32_levels: [f32; 12] = [
        0.0, 0.25, 0.5, 0.75, 1.0, -0.5, 1.5, 0.125, 0.375, 0.625, 0.875, 0.0625,
    ];
    let mut reader = seekable(attached_image(
        r#"geometry="4:3:1" sampleFormat="Float32" bounds="0:1""#,
        le_f32(&f32_levels),
    ))
    .expect("the unit constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("an advanced reader has a header");
    assert_eq!(header.sample_format(), Some(SampleFormat::F32));
    assert_eq!(*header.bounds(), astroframe::Bounds::Declared(0.0, 1.0));

    // Layer 1 first — the file's own sample type, before any normalization.
    let mut native = Samples::zeroed(SampleFormat::F32, 12);
    reader
        .read_samples_into(&mut native)
        .expect("native samples decode");
    match &native {
        Samples::F32(v) => assert_same_bits(v, &f32_levels, "Float32 native samples"),
        other => panic!("expected F32 samples, got {other:?}"),
    }

    // Then layer 2, over the declared range.
    let image = reader.read_image().expect("the image normalizes");
    assert_same_bits(
        &image.into_samples(),
        &expected_f32(&f32_levels, 0.0, 1.0),
        "Float32 normalized",
    );
}

/// §7.2's *monolithic files* and *multiple `Image` elements from one file* bullets.
///
/// The corpus makes the second concrete: one master holds two images of different geometry
/// **and** different sample format in the same file.
#[test]
fn baseline_conformance_reads_several_images_of_different_shapes_from_one_monolithic_file() {
    let first = repeating_u16(12);
    let second: [u8; 6] = [0, 51, 102, 153, 204, 255];
    let bytes = Unit::new()
        .attached(&image_element(r#"id="frame""#), le_u16(&first))
        .attached(
            r#"<Image geometry="3:2:1" sampleFormat="UInt8" id="crop_mask" {loc}/>"#,
            le_u8(&second),
        )
        .build();

    let mut reader = seekable(bytes).expect("the unit constructs");

    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("a header");
    assert_eq!(header.image_id(), Some("frame"));
    assert_eq!(header.sample_format(), Some(SampleFormat::U16));
    let image = reader.read_image().expect("the first image decodes");
    assert_same_bits(&image.into_samples(), &expected_u16(&first), "first image");

    assert!(reader.next_image().expect("the walk advances again"));
    let header = reader.header().expect("a header");
    assert_eq!(header.image_id(), Some("crop_mask"));
    assert_eq!(header.sample_format(), Some(SampleFormat::U8));
    assert_eq!(
        (header.width(), header.height(), header.channels()),
        (Some(3), Some(2), Some(1))
    );
    let image = reader.read_image().expect("the second image decodes");
    assert_same_bits(&image.into_samples(), &expected_u8(&second), "second image");

    // A single-image source returns `true` then `false`; this one returns `true` twice.
    assert!(!reader.next_image().expect("the walk ends"));
}

// ------------------------ *Header-only decode works on a truncated prefix* (XISF half)

/// Criterion *Header-only decode works on a truncated prefix*.
///
/// A consumer that fetches a size-capped prefix of a remote file depends on this, and it is
/// easy to break by validating a declared block offset against a length the source does not
/// have — the check has to fire when the block is *reached*, not when it is declared.
#[test]
fn a_header_only_prefix_yields_a_complete_header_with_no_error() {
    let levels = samples();
    let unit = Unit::new().attached(
        &image_element(r#"id="prefix" imageType="Light" offset="3""#),
        le_u16(&levels),
    );
    let prefix = unit.build_header_only();
    assert!(
        prefix.len() < unit.build().len(),
        "the prefix really is shorter than the file"
    );

    let mut reader = seekable(prefix).expect("a header-only prefix constructs");
    assert!(reader.next_image().expect("the walk advances"));
    let header = reader.header().expect("a complete Header");
    // Complete: the geometry three, the sample format, and the reported attributes, with no
    // decline — the declared offset lies past the end of this source and is not checked here.
    assert!(header.decline_reason().is_none(), "{header:?}");
    assert_eq!(
        (header.width(), header.height(), header.channels()),
        (Some(4), Some(3), Some(1))
    );
    assert_eq!(header.sample_format(), Some(SampleFormat::U16));
    assert_eq!(header.image_id(), Some("prefix"));
    assert_eq!(header.image_type(), Some(&ImageType::Light));
    assert_eq!(header.offset(), Some(3.0));
    assert_eq!(header.granularity(), Granularity::Rows);

    // The other half of the same rule, which is what keeps the check from being merely
    // deferred into nonexistence: reaching the block on a known-length source refuses it.
    let err = reader
        .read_image_into(&mut [0.0; 12])
        .expect_err("the block is not in the prefix");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

// ------------------------------------------------------------------ header encoding

/// Row *XML entity references are resolved … Duplicate attributes on one element are rejected
/// rather than last-wins*, second half.
///
/// Last-wins would let a writer — or an attacker — state a geometry twice and have the decoder
/// pick one, which is exactly the silent divergence between two readers this refuses.
#[test]
fn an_element_carrying_one_attribute_twice_is_malformed_at_construction() {
    let unit = xml_unit(
        r#"<Image geometry="4:3:1" geometry="2:6:1" sampleFormat="UInt16" location="embedded"/>"#,
    );
    let err = seekable(unit).expect_err("a duplicated attribute");
    // A unit-level fault: the header did not parse, so there is no position to decline.
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// The same row, extended: attributes never inherit a default namespace the way elements do
/// (XML Namespaces §5.2), and every core XISF attribute is written unprefixed — see
/// `a_namespace_prefixed_header_decodes` above, where even a fully `pi:`-prefixed `Image`
/// keeps bare `geometry`/`sampleFormat`/`location`. So a prefix on an attribute names
/// something genuinely foreign, not a shorthand in the same namespace the way a prefixed
/// *element* is, and `a:x`/`b:x` are two distinct attributes rather than one written twice.
/// `read_attributes` keys duplicate detection (and storage) on the full qualified name — unless
/// that prefix resolves into the XISF namespace, which is a separate rule this test does not
/// exercise (see `a_prefixed_attribute_bound_to_the_xisf_namespace_decodes`) — not the local
/// name `local_name` reduces element names to, so this decodes rather than being rejected as a
/// false duplicate.
///
/// Both prefixes are *declared*, on the `Image` element itself, to two distinct foreign
/// namespaces: an undeclared prefix would resolve to nothing at all and take the same
/// "foreign" branch by coincidence rather than by the rule this test is meant to pin.
#[test]
fn attributes_with_different_prefixes_and_the_same_local_name_do_not_collide() {
    let levels = samples();
    let stored = le_u16(&levels);
    let unit = embedded_u16(
        r#"xmlns:a="http://example.invalid/a" xmlns:b="http://example.invalid/b" a:x="1" b:x="2""#,
        r#"encoding="base64""#,
        &base64(&stored),
    );
    decodes_to(
        unit,
        &levels,
        "differently prefixed attributes sharing a local name, both prefixes declared",
    );
}

/// The other half: two attributes with the *same* qualified name, prefix included, are still
/// one attribute written twice — with the prefix declared, for the same reason as above.
#[test]
fn two_attributes_with_the_same_qualified_name_are_still_a_duplicate() {
    let unit = xml_unit(
        r#"<Image xmlns:a="http://example.invalid/a" geometry="4:3:1" sampleFormat="UInt16" location="embedded" a:x="1" a:x="2">
             <Data encoding="base64">AA==</Data>
           </Image>"#,
    );
    let err = seekable(unit).expect_err("the same qualified name repeated");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// Row *Header encoding is UTF-8 (§9.5); invalid UTF-8 is `Malformed`*, and the tolerated
/// missing declaration is covered above.
#[test]
fn invalid_utf8_in_the_header_region_is_malformed_at_construction() {
    let levels = samples();
    let stored = le_u16(&levels);
    // A well-formed header, then one byte of an attribute value replaced by a lone `0xff` —
    // which is valid nowhere in UTF-8, so no transcoding guess could rescue it.
    let good = located_u16(|p, s| format!("attachment:{p}:{s}"), &stored);
    let marker = good
        .windows(6)
        .position(|w| w == b"UInt16")
        .expect("the fixture names its sample format");
    let mut broken = good;
    broken[marker + 2] = 0xff;
    let err = seekable(broken).expect_err("a lone 0xff in the header");
    assert_eq!(kind(&err), "Malformed", "{err}");
}

/// The other half of the same row: **a declared non-UTF-8 encoding is `Unsupported`**.
///
/// `quick-xml` is built without its transcoding feature, so honouring such a declaration is
/// not on the table; reading the bytes as UTF-8 anyway is the guess the row says is worse than
/// refusing, because every byte above `0x7f` would then mean something other than what the
/// file said it meant.
#[test]
fn a_declared_non_utf8_header_encoding_is_unsupported() {
    let levels = samples();
    let stored = le_u16(&levels);
    let unit = attached_unit(
        |position, size| {
            format!(
                r#"<?xml version="1.0" encoding="ISO-8859-1"?><xisf{ROOT}><Image geometry="4:3:1" sampleFormat="UInt16" location="attachment:{position}:{size}"/></xisf>"#
            )
        },
        &stored,
    );
    let err = seekable(unit).expect_err("a declared non-UTF-8 encoding");
    assert_eq!(kind(&err), "Unsupported", "{err}");
}
