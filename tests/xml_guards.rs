//! Criterion *The XML guards, each with its own test*, and the XISF half of *Every cap has a
//! test that trips it*.
//!
//! `quick-xml` gives a Rust implementation none of what Go's standard XML parser gives a Go
//! implementation for free, so every row of § Hardening's guard table is an explicit guard here and
//! gets an explicit test. The FITS caps are graded in `tests/fits_caps.rs`; this file covers
//! the ones no other file trips — `xml_header_bytes`, `xml_depth`, `xml_elements`,
//! `subblock_count`, `attributes_per_element`, `attribute_value_bytes`,
//! `stored_block_multiple`/`stored_block_floor_bytes` and `zstd_window_bytes`.
//!
//! **Two of these tests are claims about the dependency rather than about this crate**, and
//! that is why they exist: `quick-xml`'s default resolver handles only the five predefined XML
//! entities and does not resolve recursively, and its DTD handling *skips* the internal subset
//! rather than processing it — so a declared entity is never defined and billion-laughs is not
//! expressible. Both properties were checked against the pinned version's source; these tests
//! pin them observably, because a dependency bump is exactly the event that would change them
//! without anything here changing.
//!
//! **Nothing here is allowed to pass vacuously.** Every cap is exercised in both directions:
//! a fixture that trips it is also shown to decode — or at least to get past that check — with
//! the cap raised, so a fixture malformed for an unrelated reason cannot pass for the wrong
//! reason.

#![forbid(unsafe_code)]

mod common;

use std::io::Cursor;

use astroframe::{Error, Limits, Reader};

use common::xisf::{PREAMBLE, Unit, le_u16, lz4, raw_unit, repeating_u16};
use common::{CountingRead, kind};

// ------------------------------------------------------------------ helpers

fn assert_names(case: &str, e: &Error, fragment: &str) {
    let text = e.to_string();
    assert!(
        text.contains(fragment),
        "{case}: the error should name {fragment:?}, got {text:?}"
    );
}

/// Construct a reader over the bytes, which is where every guard in the XML parser fires: an
/// unparseable or over-guarded header is a **unit-level** fault, so no `Reader` exists at all
/// and the caller has nothing to walk.
fn construct(bytes: &[u8], limits: Limits) -> astroframe::Result<Reader<impl astroframe::Source>> {
    Reader::seekable_with_limits(Cursor::new(bytes), limits)
}

/// A guard that fires during the header parse: `LimitExceeded` (or `Malformed`), and **no
/// reader**. The `Ok` arm is the second half of that assertion.
fn expect_at_construction(case: &str, bytes: &[u8], limits: Limits, want: &'static str) -> Error {
    match construct(bytes, limits) {
        Ok(_) => panic!("{case}: expected {want} and no reader; construction succeeded"),
        Err(e) => {
            assert_eq!(kind(&e), want, "{case}: {e}");
            e
        }
    }
}

/// A guard that declines the *position* rather than the unit: construction and advancing both
/// succeed, and the pixel call is where it surfaces.
fn expect_at_pixels(case: &str, bytes: &[u8], limits: Limits, want: &'static str) -> Error {
    let mut reader = construct(bytes, limits).unwrap_or_else(|e| panic!("{case}: construct: {e}"));
    assert!(
        reader
            .next_image()
            .unwrap_or_else(|e| panic!("{case}: advance: {e}")),
        "{case}: the fixture must reach an image position"
    );
    match reader.read_image() {
        Ok(image) => panic!(
            "{case}: expected {want} and no value; got {} samples",
            image.samples().len()
        ),
        Err(e) => {
            assert_eq!(kind(&e), want, "{case}: {e}");
            e
        }
    }
}

fn decodes(case: &str, bytes: &[u8], limits: Limits, want: &[u16]) {
    let mut reader = construct(bytes, limits).unwrap_or_else(|e| panic!("{case}: construct: {e}"));
    assert!(
        reader
            .next_image()
            .unwrap_or_else(|e| panic!("{case}: advance: {e}"))
    );
    let image = reader
        .read_image()
        .unwrap_or_else(|e| panic!("{case}: the cap-raised direction must decode: {e}"));
    assert_eq!(image.samples().len(), want.len(), "{case}: sample count");
    for (i, (got, level)) in image.samples().iter().zip(want).enumerate() {
        let expected = *level as f32 * (1.0f32 / 65535.0f32);
        assert_eq!(
            got.to_bits(),
            expected.to_bits(),
            "{case}: sample {i}: got {got:?}, want {expected:?}"
        );
    }
}

/// A unit whose header is `body` wrapped in a well-formed root, with no attachment.
fn header_unit(body: &str) -> Vec<u8> {
    Unit::new().xml(body).build()
}

/// A unit whose header is exactly `header` — for the fixtures whose whole point is header text
/// the builder would refuse to produce.
fn unit(header: &str) -> Vec<u8> {
    raw_unit(
        b"XISF0100",
        u32::try_from(header.len()).expect("a fixture header fits u32"),
        header,
        &[],
    )
}

// ================================================================== the guard table

/// § Hardening, row 1: *Declared header length — rejected above the cap **before** the read.*
///
/// "Before the read" is the load-bearing half and is asserted directly: the source is wrapped
/// in a counter, and nothing past the 16-byte preamble may be pulled from it. The XML header is
/// the one deliberate exception to "geometry is the ceiling", having no geometry to be checked
/// against, so this cap plus the incremental read is the whole of what keeps a declared size
/// from ever becoming an allocation.
#[test]
fn a_declared_header_length_above_the_cap_is_rejected_before_the_read() {
    let limits = Limits::default();
    let declared = u32::try_from(limits.xml_header_bytes + 1).expect("the default cap fits u32");
    let bytes = raw_unit(b"XISF0100", declared, "", &[]);

    let (source, counter) = CountingRead::new(Cursor::new(bytes.clone()));
    let e = match Reader::seekable(source) {
        Ok(_) => panic!("an over-cap header length must yield no reader"),
        Err(e) => e,
    };
    assert_eq!(kind(&e), "LimitExceeded", "{e}");
    assert_names("declared header length", &e, "XML header length");
    assert_eq!(
        counter.get(),
        PREAMBLE as u64,
        "the declared length is refused before the header region is read"
    );

    // The other direction. A cap that admits the length turns the same bytes into a truncation,
    // which is what shows the cap — not the fixture — was the check that fired.
    let mut raised = Limits::default();
    raised.xml_header_bytes = u64::from(declared);
    let e = expect_at_construction(
        "declared header length (raised)",
        &bytes,
        raised,
        "Malformed",
    );
    assert_names("declared header length (raised)", &e, "truncated");
}

/// § Hardening, row 2: *`DOCTYPE` — rejected outright.*
///
/// No legitimate XISF header carries one, and refusing it removes DTDs, entity declarations and
/// XXE as a category in one rule. Nothing about the document's shape can make it acceptable, so
/// there is no raised-cap direction to run: the both-directions check is that the identical
/// header **without** the DOCTYPE parses.
#[test]
fn a_doctype_declaration_is_rejected_outright() {
    const DECLARATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;
    const DOCTYPE: &str = r#"<!DOCTYPE xisf SYSTEM "http://example.invalid/xisf.dtd">"#;
    const ROOT: &str = r#"<xisf xmlns="http://www.pixinsight.com/xisf" version="1.0"></xisf>"#;

    let with_doctype = unit(&format!("{DECLARATION}{DOCTYPE}{ROOT}"));
    let e = expect_at_construction("DOCTYPE", &with_doctype, Limits::default(), "Malformed");
    assert_names("DOCTYPE", &e, "DOCTYPE");

    // The other direction, differing in exactly the declaration: the identical header without
    // it parses, so the refusal is attributable to the DOCTYPE and to nothing else about the
    // document's shape.
    let without = unit(&format!("{DECLARATION}{ROOT}"));
    construct(&without, Limits::default()).expect("the same header without a DOCTYPE parses");

    // And a real unit still decodes, so the guard has not simply broken header parsing.
    let levels = repeating_u16(4);
    let clean = Unit::new().image_u16(2, 2, 1, &levels).build();
    decodes(
        "DOCTYPE (an ordinary unit)",
        &clean,
        Limits::default(),
        &levels,
    );
}

/// § Hardening, row 3: *Entity expansion — cannot amplify.*
///
/// A billion-laughs header needs a DOCTYPE to declare its entities, and the DOCTYPE is refused
/// before any of them is read. That is the mechanism, and it is asserted as such rather than
/// merely observing that the file fails.
#[test]
fn a_billion_laughs_header_is_rejected() {
    let header = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<!DOCTYPE lolz ["#,
        r#"<!ENTITY lol "lol">"#,
        r#"<!ENTITY lol1 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">"#,
        r#"<!ENTITY lol2 "&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;">"#,
        r#"<!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">"#,
        r#"<!ENTITY lol4 "&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;">"#,
        r#"<!ENTITY lol5 "&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;">"#,
        r#"<!ENTITY lol6 "&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;">"#,
        r#"<!ENTITY lol7 "&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;">"#,
        r#"<!ENTITY lol8 "&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;">"#,
        r#"<!ENTITY lol9 "&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;">"#,
        r#"]>"#,
        r#"<xisf xmlns="http://www.pixinsight.com/xisf" version="1.0">&lol9;</xisf>"#,
    );
    let bytes = unit(header);
    let e = expect_at_construction("billion laughs", &bytes, Limits::default(), "Malformed");
    assert_names("billion laughs", &e, "DOCTYPE");
}

/// The dependency half of § Hardening's entity-expansion row, pinned observably because it
/// belongs to `quick-xml` rather than to this crate.
///
/// Three separate claims, three separate observations:
///
/// 1. **Only the five predefined entities resolve.** A reference to any other name is an error
///    naming it, rather than an empty expansion or a passthrough.
/// 2. **Resolution does not recurse.** `&amp;lt;` yields the four characters `&lt;`, not `<`.
///    A recursive resolver is what makes an entity bomb amplify at all.
/// 3. **No declaration can define an entity here**, because the only place to write one is a
///    DOCTYPE and that is refused — which is what makes billion-laughs *inexpressible* rather
///    than merely capped.
#[test]
fn quick_xml_resolves_only_the_five_predefined_entities_and_does_not_recurse() {
    // 1. An undeclared entity is an error naming the entity.
    let bytes = header_unit(r#"<Image geometry="2:2:1" sampleFormat="UInt16" id="&lol;"/>"#);
    let e = expect_at_construction("undeclared entity", &bytes, Limits::default(), "Malformed");
    assert_names("undeclared entity", &e, "lol");

    // The five that do resolve, each observed in a reported value.
    let bytes = header_unit(
        r#"<Image geometry="2:2:1" sampleFormat="UInt16" id="&lt;&gt;&amp;&quot;&apos;"/>"#,
    );
    let mut reader = construct(&bytes, Limits::default()).expect("the five predefined resolve");
    assert!(reader.next_image().expect("advance"));
    assert_eq!(
        reader.current_header().expect("a header").image_id(),
        Some("<>&\"'"),
        "the five predefined XML entities resolve, and nothing else does"
    );

    // 2. No recursion: the resolver runs once. `&amp;lt;` is `&` followed by `lt;`, and a
    //    resolver that ran again would turn the result into `<`.
    let bytes = header_unit(r#"<Image geometry="2:2:1" sampleFormat="UInt16" id="&amp;lt;"/>"#);
    let mut reader = construct(&bytes, Limits::default()).expect("one resolution pass succeeds");
    assert!(reader.next_image().expect("advance"));
    assert_eq!(
        reader.current_header().expect("a header").image_id(),
        Some("&lt;"),
        "entity resolution does not recurse; a second pass would yield \"<\""
    );

    // 3. A DOCTYPE declaring the entity does not define it, because the DOCTYPE never gets
    //    that far. The internal subset `quick-xml` skips is therefore unreachable by
    //    construction rather than merely unprocessed.
    let header = concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<!DOCTYPE xisf [<!ENTITY lol "lol">]>"#,
        r#"<xisf xmlns="http://www.pixinsight.com/xisf" version="1.0">&lol;</xisf>"#,
    );
    let bytes = unit(header);
    let e = expect_at_construction(
        "a declaration cannot define an entity",
        &bytes,
        Limits::default(),
        "Malformed",
    );
    assert_names("a declaration cannot define an entity", &e, "DOCTYPE");
}

/// § Hardening, row 4: *Element nesting depth — capped.*
///
/// Go's parser skips unknown subtrees iteratively; a Rust parser that recurses can be made to
/// overflow its stack, which is an **abort** rather than an error and breaks the no-panic
/// contract. The fixture nests far past the cap — 5000 levels, deep enough that a recursive
/// descent would run out of stack — so what this asserts is that an error comes back at all.
#[test]
fn a_header_nested_past_the_depth_cap_is_rejected_rather_than_overflowing_the_stack() {
    let limits = Limits::default();
    let deep = nested(5000);
    let bytes = header_unit(&deep);
    let e = expect_at_construction("nesting depth", &bytes, limits, "LimitExceeded");
    assert_names("nesting depth", &e, "nesting depth");

    // The other direction, at the boundary: the root counts as one level, so the deepest
    // header the cap admits nests `xml_depth - 1` elements inside it.
    let admitted = header_unit(&nested(limits.xml_depth as usize - 1));
    construct(&admitted, limits).expect("a header at exactly the depth cap parses");
    let refused = header_unit(&nested(limits.xml_depth as usize));
    let e = expect_at_construction(
        "nesting depth (boundary)",
        &refused,
        limits,
        "LimitExceeded",
    );
    assert_names("nesting depth (boundary)", &e, "nesting depth");
}

fn nested(levels: usize) -> String {
    let mut out = String::with_capacity(levels * 8);
    for _ in 0..levels {
        out.push_str("<a>");
    }
    for _ in 0..levels {
        out.push_str("</a>");
    }
    out
}

/// § Hardening, row 6: *Total element count — capped.*
///
/// Without it a header full of `FITSKeyword` elements allocates a struct per element up to the
/// header cap. The prior decoder has no such cap; a review of it flagged the absence.
#[test]
fn a_header_past_the_element_count_cap_is_rejected() {
    let limits = Limits::default();
    let over = limits.xml_elements + 1;
    let bytes = header_unit(&"<a/>".repeat(over as usize));
    let e = expect_at_construction("element count", &bytes, limits, "LimitExceeded");
    assert_names("element count", &e, "total element count");

    // The other direction: the identical header parses under a cap that admits it, so the
    // count — not the shape — is what refused it.
    let mut raised = Limits::default();
    raised.xml_elements = over + 1;
    construct(&bytes, raised).expect("the same header parses under a raised element cap");
}

/// § Hardening, row 5, first half: *Attribute count per element — capped.*
///
/// The subblock list earns its own checks precisely because the whole list is one attribute
/// string rather than elements; that reasoning generalizes to one element carrying a million
/// attributes.
#[test]
fn an_element_with_too_many_attributes_is_rejected() {
    let limits = Limits::default();
    let bytes = header_unit(&attributed(limits.attributes_per_element as usize + 1));
    let e = expect_at_construction("attributes per element", &bytes, limits, "LimitExceeded");
    assert_names("attributes per element", &e, "attributes per element");

    // The other direction, at the boundary: exactly the cap parses.
    let admitted = header_unit(&attributed(limits.attributes_per_element as usize));
    construct(&admitted, limits).expect("an element at exactly the attribute cap parses");
}

fn attributed(count: usize) -> String {
    let mut out = String::from("<a");
    for i in 0..count {
        out.push_str(&format!(" a{i}=\"1\""));
    }
    out.push_str("/>");
    out
}

/// § Hardening, row 5, second half: *Attribute-value length — capped.*
///
/// The same reasoning reaches a single 8 MiB attribute value, which no element count bounds.
#[test]
fn an_over_long_attribute_value_is_rejected() {
    let limits = Limits::default();
    let over = limits.attribute_value_bytes as usize + 1;
    let bytes = header_unit(&format!("<a v=\"{}\"/>", "x".repeat(over)));
    let e = expect_at_construction("attribute value length", &bytes, limits, "LimitExceeded");
    assert_names("attribute value length", &e, "attribute value length");

    // The other direction, at the boundary: a value of exactly the cap parses.
    let admitted = header_unit(&format!(
        "<a v=\"{}\"/>",
        "x".repeat(limits.attribute_value_bytes as usize)
    ));
    construct(&admitted, limits).expect("a value at exactly the cap parses");
}

/// The `keyword_value_bytes` cap — the one cap that bounds a reported value against **how it
/// was reached** rather than how it was written.
///
/// §4.2.1.2 exists to assemble a long string from short records, and XISF lets a `<Reference>`
/// reach one continuation record many times, so the assembled value grows by that record's
/// length *per reference*. 2048 references to one 500 KB record assemble a gigabyte from a
/// 553 KB header — 7590x the input, at tier 1. Nothing about sharing closes it: the assembled
/// string genuinely is that long, which is what makes a cap the only answer.
///
/// The chain here is small and the cap is lowered to meet it, so the test costs nothing; the
/// shape is what matters, not the size.
#[test]
fn the_assembled_keyword_value_cap_trips() {
    let mut limits = Limits::default();
    limits.keyword_value_bytes = 64;

    // One continuation record, reached eight times. Each reference legitimately continues the
    // chain, so the assembled value is eight times the record's own length.
    let piece = "y".repeat(32);
    let unit = |refs: usize| {
        header_unit(&format!(
            r#"<FITSKeyword uid="c" name="CONTINUE" value="'{piece}&amp;'" comment=""/><Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data><FITSKeyword name="LONGSTR" value="'x&amp;'" comment=""/>{}</Image>"#,
            r#"<Reference ref="c"/>"#.repeat(refs)
        ))
    };

    let e = expect_at_construction("assembled keyword value", &unit(8), limits, "LimitExceeded");
    assert_names("assembled keyword value", &e, "assembled keyword value");

    // The other direction, and it is the one that matters here: the *same record* reached
    // once assembles well under the cap, so what trips it is the multiplication rather than
    // the record.
    construct(&unit(1), limits).expect("one continuation stays under the cap");
}

// ================================================================== the remaining XISF caps

/// The `subblock_count` cap. §10.6 requires no validation of the list and explicitly sets **no**
/// upper limit on the number of subblocks, so without this the attribute is a cheap
/// amplification vector the element-count cap does not cover — the whole list being one
/// attribute string rather than elements.
#[test]
fn the_subblock_count_cap_trips() {
    let limits = Limits::default();
    let over = limits.subblock_count as usize + 1;
    let list = (0..over)
        .map(|_| "1,1".to_owned())
        .collect::<Vec<_>>()
        .join(":");
    let bytes = attach_image(
        &format!(
            r#"geometry="2:2:1" sampleFormat="UInt16" compression="lz4:8" subblocks="{list}""#
        ),
        vec![0u8; 8],
    );
    let e = expect_at_pixels("subblock count", &bytes, limits, "LimitExceeded");
    assert_names("subblock count", &e, "subblock count");

    // Both directions, on a list that is otherwise entirely valid: two subblocks decode under
    // the default cap and are refused under a cap of one.
    let levels = repeating_u16(8);
    let raw = le_u16(&levels);
    let (front, back) = raw.split_at(8);
    let mut block = lz4(front);
    let first = block.len();
    block.extend_from_slice(&lz4(back));
    let second = block.len() - first;
    let bytes = attach_image(
        &format!(
            r#"geometry="4:2:1" sampleFormat="UInt16" compression="lz4:16" subblocks="{first},8:{second},8""#
        ),
        block,
    );
    decodes("subblock count (raised)", &bytes, limits, &levels);

    let mut tight = Limits::default();
    tight.subblock_count = 1;
    let e = expect_at_pixels("subblock count (lowered)", &bytes, tight, "LimitExceeded");
    assert_names("subblock count (lowered)", &e, "subblock count");
}

/// The **stored-block cap**, which closes the one remaining hole in invariant I4 and is
/// therefore the last cap that should go untested.
///
/// A declared `attachment:pos:size` becomes an allocation, because a block that cannot stream
/// must be fully resident before it decompresses — and a compressed block may legitimately
/// exceed its uncompressed size, so the geometry cross-check alone cannot bound it. On a
/// seekable source the file length bounds it incidentally, which is exactly why this is graded
/// over **`Reader::sequential`**: a pipe has no file length at all, and that is the second
/// consumer's shape.
///
/// The block is deliberately zlib at *stored* level, so its compressed form is larger than its
/// uncompressed form — the real case the cap exists for, rather than a size the header simply
/// lied about.
#[test]
fn the_stored_block_cap_trips_on_a_sequential_source() {
    let levels = repeating_u16(64 * 64);
    let raw = le_u16(&levels);
    let stored = zlib_stored(&raw);
    assert!(
        stored.len() > raw.len(),
        "the fixture needs a stored block larger than its uncompressed size, got {} against {}",
        stored.len(),
        raw.len()
    );

    let bytes = attach_image(
        &format!(
            r#"geometry="64:64:1" sampleFormat="UInt16" compression="zlib:{}""#,
            raw.len()
        ),
        stored.clone(),
    );

    // Under the defaults the cap is `max(implied x 2, 1 MiB)` and the block is nowhere near it,
    // so the file decodes — over a forward-only source, with no length to fall back on.
    let limits = Limits::default();
    let mut reader =
        Reader::sequential_with_limits(Cursor::new(&bytes[..]), limits).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let image = reader
        .read_image()
        .expect("the block decodes under the default cap");
    for (i, (got, level)) in image.samples().iter().zip(&levels).enumerate() {
        let want = *level as f32 * (1.0f32 / 65535.0f32);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "stored block cap: sample {i}"
        );
    }

    // Lowered so that the cap lands between the geometry-implied size and the stored size:
    // both fields of the row are exercised, the multiple and the floor beneath it.
    let mut tight = Limits::default();
    tight.stored_block_multiple = 1;
    tight.stored_block_floor_bytes = raw.len() as u64;
    let mut reader =
        Reader::sequential_with_limits(Cursor::new(&bytes[..]), tight).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let e = match reader.read_image() {
        Ok(image) => panic!(
            "stored block cap: expected LimitExceeded and no value; got {} samples",
            image.samples().len()
        ),
        Err(e) => e,
    };
    assert_eq!(kind(&e), "LimitExceeded", "stored block cap: {e}");
    assert_names("stored block cap", &e, "stored block bytes");
}

/// The `zstd_window_bytes` cap. A zstd decoder allocates the window its frame header declares
/// **before producing a byte** — the precise shape of "an allocation sized from an unvalidated
/// declared size", and the reason zstd is the only codec carrying this cap: zlib is discharged
/// by its fixed ~32 KiB window and LZ4 has none.
///
/// The two fixtures differ in exactly one byte, the frame's window descriptor, so the class
/// flip is attributable to the declared window and to nothing else. Raising the cap instead
/// would allocate the gigabyte the cap exists to refuse.
#[test]
fn the_zstd_declared_window_cap_trips() {
    let limits = Limits::default();

    // Window log 30 — a declared 1 GiB window, far above the 8 MiB default.
    let bytes = attach_image(
        r#"geometry="2:2:1" sampleFormat="UInt16" compression="zstd:8""#,
        zstd_frame_header(WINDOW_LOG_30),
    );
    let e = expect_at_pixels("zstd declared window", &bytes, limits, "LimitExceeded");
    assert_names("zstd declared window", &e, "zstd decoder window");

    // The identical frame with window log 10 — a 1 KiB window — gets past the cap and fails on
    // the frame having no data behind its header, which is a different check and a different
    // class.
    let bytes = attach_image(
        r#"geometry="2:2:1" sampleFormat="UInt16" compression="zstd:8""#,
        zstd_frame_header(WINDOW_LOG_10),
    );
    let e = expect_at_pixels("zstd declared window (small)", &bytes, limits, "Malformed");
    assert_ne!(
        kind(&e),
        "LimitExceeded",
        "a small declared window must not trip the cap"
    );
}

// ------------------------------------------------------------------ fixtures

fn attach_image(attrs: &str, block: Vec<u8>) -> Vec<u8> {
    Unit::new()
        .attached(&format!("<Image {attrs} {{loc}}/>"), block)
        .build()
}

/// zlib at *stored* level: the deflate blocks carry the input verbatim, so the compressed form
/// is a few bytes **larger** than the uncompressed one. `tests/common/xisf.rs`'s `zlib` uses the
/// default level, which is the wrong shape for a cap whose whole subject is a compressed block
/// exceeding its uncompressed size.
fn zlib_stored(input: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write as _;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::none());
    encoder.write_all(input).expect("zlib encode");
    encoder.finish().expect("zlib finish")
}

/// A zstd `Window_Descriptor` byte: `Exponent << 3 | Mantissa`, for `Window_Size = (1 <<
/// (10 + Exponent)) + (1 << (10 + Exponent)) / 8 * Mantissa`.
const WINDOW_LOG_30: u8 = 20 << 3;
const WINDOW_LOG_10: u8 = 0;

/// A zstd frame header alone, written byte by byte from RFC 8878 §3.1.1: the four-byte magic,
/// a `Frame_Header_Descriptor` of zero — no content size, not single-segment, no checksum, no
/// dictionary — and the `Window_Descriptor` the caller chose. Nothing follows it, which is
/// deliberate: a decoder must reject the declared window **before** it reads a block.
fn zstd_frame_header(window: u8) -> Vec<u8> {
    vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, window]
}
