//! **Baseline XISF decoder conformance, graded bullet by bullet (XISF §7.2).**
//!
//! §7 defines a *baseline conformance* level and says that any decoder claiming conformance
//! with the specification *shall* satisfy it. This file is the demonstration: one test per
//! bullet of §7.2's list, named after the bullet it grades, so the file **is** the checklist.
//! A reader asking "does this crate meet §7.2?" should not have to assemble an answer from
//! evidence scattered across the suite, and a future revision that adds a bullet should leave
//! a visible hole here rather than an argument somewhere else.
//!
//! §7.2's ten abilities, and where each is graded:
//!
//! | Ability | Test |
//! | --- | --- |
//! | Read monolithic XISF files | `reads_several_images_of_different_shapes_from_one_monolithic_file` |
//! | Read 8/16/32/64-bit scalar properties | `scalar_properties_of_every_width` |
//! | Read multiple `Image` elements from one file | `reads_several_images_of_different_shapes_from_one_monolithic_file` |
//! | Read inline, embedded and attachment block locations | `reads_both_pixel_locations_that_an_image_may_use` — partial, see below |
//! | Read little-endian and big-endian data blocks | `data_blocks_in_both_byte_orders` |
//! | Decompress all standard codecs | `reads_every_standard_codec_and_its_shuffled_variant` |
//! | Verify SHA-1, SHA-256 and SHA-512 checksums | `the_three_checksum_algorithms_every_decoder_must_verify` |
//! | Read planar and normal pixel storage | `reads_planar_and_normal_storage_in_gray_and_rgb` |
//! | Read `UInt8`, `UInt16` and `Float32` samples | `reads_uint8_uint16_and_float32_samples` |
//! | Read grayscale and RGB colour spaces | `reads_planar_and_normal_storage_in_gray_and_rgb` |
//!
//! Plus the two of §7's general obligations that bind a decoder whatever else it supports —
//! the third governs encoders: `an_unsupported_feature_leaves_the_rest_of_the_unit_accessible`
//! and `unrecognized_elements_and_attributes_are_ignored`.
//!
//! **The inline bullet cannot be met as written, and that is the specification's own
//! inconsistency rather than a gap here.** §7.2 lists inline among the block locations a
//! baseline decoder reads pixel data from, while §11.5 states that "An Image element cannot
//! serialize pixel data as an inline XISF data block", the restriction being that Image
//! elements can have child XML elements. Revision 1 did not resolve it. This crate follows
//! §11.5 — inline pixel data is `Malformed`, graded in `tests/xisf_decisions.rs` — and reads
//! inline blocks everywhere else they are legal.
//!
//! §7.1's baseline **encoder** bullets are not graded anywhere: this crate is decode-only and
//! claims no encoder conformance.
//!
//! Two things this file deliberately does not do. It does not grade what the crate declines —
//! `Complex32/64`, `CIELab`, geometry beyond `width:height:channels`, distributed units,
//! signature verification — because every one of those sits *above* baseline, and §7's rule
//! for them is to treat the object as unavailable and keep the unit readable, which is what
//! `tests/xisf_declines.rs` grades. And it does not restate the decision tables; the rows that
//! explain *why* each behaviour is what it is live in `tests/xisf_decisions.rs`.

#![cfg(feature = "xisf")]
#![forbid(unsafe_code)]

mod common;

use astroframe::{
    ColorSpace, DeclineClass, Granularity, PixelStorage, PropertyValue, SampleFormat, Samples,
};
use common::assert_same_bits;
#[cfg(feature = "checksum")]
use common::kind;
#[cfg(feature = "checksum")]
use common::xisf::checksum_attr;
use common::xisf::{
    Unit, attached_image, attached_u16, base64, be_u16, decodes_to, embedded_u16, expected_f32,
    expected_u8, expected_u16, image_element, le_f32, le_u8, le_u16, lz4, read_one, repeating_u16,
    samples, seekable, shuffle, zlib, zstd_raw,
};

/// The property a fixture declares under `id`, or a panic naming what it did declare.
fn property<'a>(header: &'a astroframe::Header, id: &str) -> &'a astroframe::Property {
    header
        .properties()
        .iter()
        .find(|p| p.id() == id)
        .unwrap_or_else(|| panic!("the fixture declares {id}: {:?}", header.properties()))
}

// ------------------------------------------------------------------ §7.2, bullet by bullet

/// §7.2's *every standard compression codec* bullet, plus the `zstd` this crate adds.
///
/// The three container shapes are the point: LZ4 and zstd fail in **opposite** directions, so
/// a decoder reaching for a framed LZ4 reader breaks LZ4 and one reaching for a bare-block
/// zstd reader breaks zstd. Every row here decodes to asserted pixels.
#[test]
fn reads_every_standard_codec_and_its_shuffled_variant() {
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
fn reads_both_pixel_locations_that_an_image_may_use() {
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
fn reads_planar_and_normal_storage_in_gray_and_rgb() {
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
fn reads_uint8_uint16_and_float32_samples() {
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
    let header = reader.current_header().expect("the advanced position");
    assert_eq!(header.sample_format(), Some(SampleFormat::F32));
    assert!(
        matches!(header.bounds(), astroframe::Bounds::Declared(r) if r.lo() == 0.0 && r.hi() == 1.0)
    );

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
fn reads_several_images_of_different_shapes_from_one_monolithic_file() {
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
    let header = reader.current_header().expect("a header");
    assert_eq!(header.image_id(), Some("frame"));
    assert_eq!(header.sample_format(), Some(SampleFormat::U16));
    let image = reader.read_image().expect("the first image decodes");
    assert_same_bits(&image.into_samples(), &expected_u16(&first), "first image");

    assert!(reader.next_image().expect("the walk advances again"));
    let header = reader.current_header().expect("a header");
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

/// §7.2's *properties of all 8-bit, 16-bit, 32-bit and 64-bit scalar types* bullet.
///
/// Every width in both signednesses, both floating point widths, and `Boolean` — §8.4.4.1's
/// scalar types minus the 128-bit ones, which §8.4.4.1 makes *optional*. Each is declared with
/// a value whose spelling the width makes reachable only at that width: a `UInt64` carrying
/// `18446744073709551615` and an `Int64` carrying the most negative 64-bit integer would both
/// be lost by a decoder narrowing through `i32` or `f64`.
///
/// The bullet says *read*, and this crate reports rather than interprets: the declared type
/// and the value text both reach the consumer, and the consumer parses. What conformance
/// requires is that neither is lost or altered, which is what is asserted here.
#[test]
fn scalar_properties_of_every_width() {
    let cases: [(&str, &str); 11] = [
        ("Boolean", "true"),
        ("Int8", "-128"),
        ("UInt8", "255"),
        ("Int16", "-32768"),
        ("UInt16", "65535"),
        ("Int32", "-2147483648"),
        ("UInt32", "4294967295"),
        // The two the narrowing decoder loses.
        ("Int64", "-9223372036854775808"),
        ("UInt64", "18446744073709551615"),
        ("Float32", "1.1754944e-38"),
        ("Float64", "2.2250738585072014e-308"),
    ];

    let declarations: String = cases
        .iter()
        .map(|(ty, value)| format!(r#"<Property id="Test:{ty}" type="{ty}" value="{value}"/>"#))
        .collect();
    let levels = samples();
    let bytes = Unit::new()
        .xml(&format!(
            concat!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded">"#,
                "{}",
                r#"<Data encoding="base64">{}</Data></Image>"#,
            ),
            declarations,
            base64(&le_u16(&levels)),
        ))
        .build();

    let (header, got) = read_one(bytes);
    assert_same_bits(&got, &expected_u16(&levels), "the image still decodes");

    for (ty, value) in cases {
        let declared = property(&header, &format!("Test:{ty}"));
        assert_eq!(
            format!("{:?}", declared.property_type()),
            ty,
            "the declared type of {ty} reaches the consumer"
        );
        match declared.value() {
            PropertyValue::Text(text) => assert_eq!(
                &**text, value,
                "{ty}: the value text is reported exactly as the file wrote it"
            ),
            other => panic!("{ty}: expected character data, got {other:?}"),
        }
    }
}

/// §7.2's *data blocks stored in little-endian and big-endian byte order* bullet (§10.4).
///
/// The same image in both orders, asserted to produce the same samples. A decoder that
/// ignored `byteOrder` would pass a little-endian-only fixture and fail here on every sample
/// whose two bytes differ — which is why the levels are chosen so that none is a palindrome.
#[test]
fn data_blocks_in_both_byte_orders() {
    let levels: [u16; 12] = [
        0x0102, 0x0304, 0x1000, 0x0010, 0xFF00, 0x00FF, 0xBEEF, 0xFEED, 0x1234, 0x4321, 0xABCD,
        0xDCBA,
    ];
    for level in levels {
        assert_ne!(
            level.to_le_bytes(),
            level.to_be_bytes(),
            "a palindromic level would pass whatever the decoder did with byteOrder"
        );
    }

    // Little-endian is §10.4's default and is left unstated on one fixture and declared on the
    // other, since both spellings are conforming and a decoder must agree with itself.
    let implied = decodes_to(
        attached_u16("", le_u16(&levels)),
        &levels,
        "little-endian by default",
    );
    let declared = decodes_to(
        attached_image(
            r#"geometry="4:3:1" sampleFormat="UInt16" byteOrder="little""#,
            le_u16(&levels),
        ),
        &levels,
        "little-endian declared",
    );
    let big = decodes_to(
        attached_image(
            r#"geometry="4:3:1" sampleFormat="UInt16" byteOrder="big""#,
            be_u16(&levels),
        ),
        &levels,
        "big-endian",
    );

    // All three report the same geometry, so the fixtures differ in byte order alone.
    for header in [&implied, &declared, &big] {
        assert_eq!(header.sample_format(), Some(SampleFormat::U16));
        assert_eq!(header.width(), Some(4));
    }
}

/// §7.2's *verify data block checksums computed with SHA-1, SHA-256 and SHA-512* bullet.
///
/// Revision 1 requires these three of **every** decoder — §10.5's "claiming support" qualifier
/// applies to encoders — so they are the conformance-bearing ones. The two SHA-3 algorithms
/// are optional and are graded with the rest of the checksum surface in
/// `tests/xisf_decisions.rs`.
///
/// Both halves of "verify" are asserted: a matching digest decodes, and a corrupted one is
/// refused rather than passed through. A decoder that parsed the attribute and ignored it
/// would satisfy the first alone.
#[cfg(feature = "checksum")]
#[test]
fn the_three_checksum_algorithms_every_decoder_must_verify() {
    let levels = samples();
    let stored = le_u16(&levels);

    for algorithm in ["sha-1", "sha-256", "sha-512"] {
        decodes_to(
            attached_u16(&checksum_attr(algorithm, &stored), stored.clone()),
            &levels,
            algorithm,
        );

        // The same digest over different bytes: present, well-formed, and wrong.
        let wrong = checksum_attr(algorithm, &le_u16(&[7u16; 12]));
        let mut reader =
            seekable(attached_u16(&wrong, stored.clone())).expect("the header still parses");
        assert!(reader.next_image().expect("the walk advances"));
        // A correctly sized destination, so the refusal is the digest rather than the call:
        // a zero-length buffer is `InvalidRequest` before any block is read.
        let err = reader
            .read_image()
            .expect_err("a mismatched digest is refused");
        assert_eq!(kind(&err), "ChecksumMismatch", "{algorithm}: {err}");
    }
}

// ------------------------------------------------------------- §7's general obligations

/// §7: a decoder that finds a feature it does not support *shall* treat the affected object as
/// unavailable and *shall* keep the rest of the XISF unit accessible.
///
/// The unsupported feature here is a compression codec no version of the specification
/// defines. The obligation has three parts and all three are asserted: the unit opens, the
/// affected image reports its decline rather than decoding, and the *next* image — which has
/// nothing wrong with it — still decodes to asserted pixels. A decoder that failed the whole
/// unit would pass the first two of those only by failing the first.
#[test]
fn an_unsupported_feature_leaves_the_rest_of_the_unit_accessible() {
    let levels = samples();
    let stored = le_u16(&levels);
    let bytes = Unit::new()
        .attached(
            &image_element(&format!(r#"compression="brotli:{}""#, stored.len())),
            stored.clone(),
        )
        .attached(&image_element(r#"id="intact""#), stored)
        .build();

    let mut reader = seekable(bytes).expect("an unsupported codec does not fail the unit");

    assert!(reader.next_image().expect("the walk advances"));
    let declined = reader
        .current_header()
        .expect("a declined position reports");
    assert_eq!(
        declined.decline_reason().map(|r| r.class()),
        Some(DeclineClass::Unsupported),
        "the codec is unsupported, not the file malformed"
    );
    reader
        .read_image_into(&mut [])
        .expect_err("the affected object is unavailable");

    assert!(reader.next_image().expect("the walk continues"));
    let intact = reader.current_header().expect("the second header");
    assert_eq!(intact.image_id(), Some("intact"));
    assert!(intact.decline_reason().is_none());
    let image = reader
        .read_image()
        .expect("the rest of the unit is readable");
    assert_same_bits(
        &image.into_samples(),
        &expected_u16(&levels),
        "the image after an unsupported one decodes normally",
    );
}

/// §7: XML elements, XML attributes and properties a decoder does not recognize *shall* be
/// ignored.
///
/// All three kinds at once, because the obligation is one rule and a decoder can fail it
/// separately for each: an element the specification does not define, an attribute on the
/// `Image` element itself, and an element in a foreign namespace at the root — which Revision
/// 1's extension-element note says is where an extension belongs. None may disturb the decode.
#[test]
fn unrecognized_elements_and_attributes_are_ignored() {
    let levels = samples();
    let stored = le_u16(&levels);
    let bytes = Unit::new()
        .xml(r#"<ex:Provenance xmlns:ex="urn:example:extension" run="42"/>"#)
        .attached(
            concat!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" somethingNew="ignored" {loc}>"#,
                r#"<SomethingFromTheFuture answer="42"/>"#,
                r#"</Image>"#,
            ),
            stored,
        )
        .build();

    let header = decodes_to(
        bytes,
        &levels,
        "unrecognized elements and attributes are ignored",
    );
    // Ignored means ignored: the decode is unaffected, and nothing unrecognized is reported as
    // though it had been understood.
    assert_eq!(header.sample_format(), Some(SampleFormat::U16));
    assert!(header.decline_reason().is_none());
}
