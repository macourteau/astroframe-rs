//! What a build without the `checksum` feature does with a block that declares one.
//!
//! §10.5 requires a decoder to verify every data block checksum it reads, and this build
//! cannot: the three hash dependencies are not compiled in. §7 says what that means. An
//! unsupported feature makes the **affected object** unavailable and leaves the rest of the
//! XISF unit accessible — so the position declines and the walk continues, rather than the
//! source failing to open.
//!
//! The distinction this file grades is the one that is easy to lose: an embedded block's
//! contents are read during the header parse, so its checksum is reached at construction,
//! where a propagated error takes the whole unit with it including images the caller never
//! asked for. An attachment's is reached at the pixel call, where it was already contained.
//! Both must decline, and neither may fail `open`.
//!
//! This file runs only in the configuration it describes. `cargo test --all-features` skips
//! it entirely, which is why the CI lane that builds without the feature also runs the tests.

#![cfg(all(feature = "xisf", not(feature = "checksum")))]
#![forbid(unsafe_code)]

mod common;

use std::io::Cursor;

use astroframe::{DeclineClass, Reader};
use common::assert_same_bits;
use common::xisf::{Unit, base64, checksum_attr, expected_u16, le_u16, samples};

/// A unit whose first image carries a checksum this build cannot verify and whose second
/// carries none, so "the rest of the unit remains accessible" is asserted rather than assumed.
fn two_images(checksummed: &str) -> Vec<u8> {
    let levels = samples();
    let stored = le_u16(&levels);
    Unit::new()
        .xml(&format!(
            concat!(
                r#"<Image geometry="4:3:1" sampleFormat="UInt16" location="embedded" {}>"#,
                r#"<Data encoding="base64">{}</Data></Image>"#,
            ),
            checksummed,
            base64(&stored),
        ))
        .image_u16(4, 3, 1, &levels)
        .build()
}

#[test]
fn an_embedded_checksummed_block_declines_its_position_and_the_unit_still_opens() {
    let levels = samples();
    let stored = le_u16(&levels);
    let bytes = two_images(&checksum_attr("sha-1", &stored));

    // The whole point: construction succeeds. Before §7 was read this way, the error from the
    // embedded block propagated out of `open` and the second image was unreachable.
    let mut reader = Reader::seekable(Cursor::new(bytes)).expect("the unit opens");

    assert!(reader.next_image().expect("the walk advances"));
    let header = reader
        .current_header()
        .expect("a declined position reports");
    let decline = header
        .decline_reason()
        .expect("a block this build cannot verify declines its position");
    assert_eq!(
        decline.class(),
        DeclineClass::Unsupported,
        "the feature is absent, not the file wrong: {}",
        decline.reason()
    );
    // Reported as undecodable and actually undecodable — a position that claims one and does
    // the other is unusable for the batch consumer the accessor exists for.
    reader
        .read_image_into(&mut [])
        .expect_err("a declined position decodes nothing");

    // The rest of the unit is accessible, which is the §7 obligation the decline exists to
    // honour rather than merely to report.
    assert!(reader.next_image().expect("the walk continues"));
    let mut got = vec![0.0f32; levels.len()];
    reader
        .read_image_into(&mut got)
        .expect("the unchecksummed image still decodes");
    assert_same_bits(
        &got,
        &expected_u16(&levels),
        "the image after a declined one decodes normally",
    );
}

/// Every algorithm §10.5 names behaves alike here: the build lacks all of them, so the class
/// is a property of the build rather than of which algorithm the file chose.
#[test]
fn every_checksum_algorithm_declines_alike_when_the_feature_is_absent() {
    let stored = le_u16(&samples());
    for algorithm in ["sha-1", "sha-256", "sha-512", "sha3-256", "sha3-512"] {
        let bytes = two_images(&checksum_attr(algorithm, &stored));
        let mut reader = Reader::seekable(Cursor::new(bytes))
            .unwrap_or_else(|e| panic!("{algorithm}: the unit opens: {e}"));
        assert!(reader.next_image().expect("the walk advances"));
        let header = reader.current_header().expect("reports");
        assert_eq!(
            header.decline_reason().map(|r| r.class()),
            Some(DeclineClass::Unsupported),
            "{algorithm}"
        );
    }
}
