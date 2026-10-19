//! `DecodeOptions`: what it defaults to, that each finisher hands its options to the reader it
//! builds, and that the reader reports them.
//!
//! A cap that trips is the evidence a finisher applied the options, rather than a reader
//! reporting a value it never used. The cap here is `fits_header_cards`, because the
//! constructor's own header parse is subject to it: an option that reached the reader only
//! after construction would let this fixture through.

mod common;

use std::io::Cursor;

use astroframe::{DecodeOptions, EqualLengthSubblocks, Limits, Reader};

use common::{file, kind, unsigned_2x2};

/// The 2x2 fixture's header holds more than two cards, so this refuses it at construction.
fn tight() -> DecodeOptions {
    let mut limits = Limits::default();
    limits.fits_header_cards = 2;
    DecodeOptions::from(limits)
}

/// Admits the fixture, and differs from the defaults so a reader reporting the defaults
/// instead cannot pass.
fn loose() -> DecodeOptions {
    DecodeOptions::new().with_limits(Limits::default().with_images_per_source(7))
}

/// A scratch path under the platform's temp directory. Never a hardcoded one: a machine-local
/// absolute path in committed source is what the CI grep fails on.
fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "astroframe-decode-options-{}-{name}",
        std::process::id()
    ))
}

fn assert_header_cards_tripped<T>(result: astroframe::Result<T>, what: &str) {
    let Err(err) = result else {
        panic!("{what}: the card cap admitted a header past it");
    };
    assert_eq!(kind(&err), "LimitExceeded", "{what}: {err}");
    assert!(
        format!("{err}").contains("more than 2 cards"),
        "{what}: {err}"
    );
}

#[test]
fn the_defaults_are_the_default_limits() {
    assert_eq!(DecodeOptions::default().limits, Limits::default());
    assert_eq!(DecodeOptions::new(), DecodeOptions::default());
}

/// The default reading is Revision 2's own rule, and the setter changes that field alone; the
/// reader reports the reading it was built with. What each reading decodes is graded in
/// `tests/uncompressed_subblocks.rs`.
#[test]
fn the_default_reading_copies_an_equal_length_subblock() {
    assert_eq!(
        DecodeOptions::default().equal_length_subblocks,
        EqualLengthSubblocks::Uncompressed
    );
    assert_eq!(
        EqualLengthSubblocks::default(),
        EqualLengthSubblocks::Uncompressed
    );

    let legacy = loose().with_equal_length_subblocks(EqualLengthSubblocks::TryDecompress);
    assert_eq!(
        legacy.equal_length_subblocks,
        EqualLengthSubblocks::TryDecompress
    );
    assert_eq!(legacy.limits, loose().limits);
    let reader = legacy
        .seekable(Cursor::new(file(&[unsigned_2x2()])))
        .expect("constructs");
    assert_eq!(*reader.options(), legacy);
}

#[test]
fn from_limits_sets_the_limits_and_nothing_else() {
    let limits = Limits::default().with_total_samples(64 << 20);
    let options = DecodeOptions::from(limits);
    assert_eq!(options.limits, limits);
    assert_eq!(options, DecodeOptions::default().with_limits(limits));

    let converted: DecodeOptions = limits.into();
    assert_eq!(converted, options);
}

#[test]
fn sequential_applies_the_options() {
    let bytes = file(&[unsigned_2x2()]);
    assert_header_cards_tripped(tight().sequential(Cursor::new(&bytes)), "sequential");

    let reader = loose()
        .sequential(Cursor::new(&bytes))
        .expect("the fixture constructs under loose caps");
    assert_eq!(*reader.options(), loose());
    assert_eq!(*reader.limits(), loose().limits);
}

#[test]
fn seekable_applies_the_options() {
    let bytes = file(&[unsigned_2x2()]);
    assert_header_cards_tripped(tight().seekable(Cursor::new(&bytes)), "seekable");

    let reader = loose()
        .seekable(Cursor::new(&bytes))
        .expect("the fixture constructs under loose caps");
    assert_eq!(*reader.options(), loose());
    assert_eq!(*reader.limits(), loose().limits);
}

#[test]
fn open_applies_the_options() {
    let path = temp_path("open.fits");
    std::fs::write(&path, file(&[unsigned_2x2()])).expect("write the fixture");

    let tripped = tight().open(&path);
    let reader = loose().open(&path);
    std::fs::remove_file(&path).expect("remove the fixture");

    assert_header_cards_tripped(tripped, "open");
    let reader = reader.expect("the fixture constructs under loose caps");
    assert_eq!(*reader.options(), loose());
    assert_eq!(*reader.limits(), loose().limits);
}

/// The `Reader` shortcuts are the finishers on the defaults, and report them.
#[test]
fn the_reader_shortcuts_report_the_default_options() {
    let bytes = file(&[unsigned_2x2()]);
    let sequential = Reader::sequential(Cursor::new(&bytes)).expect("sequential constructs");
    assert_eq!(*sequential.options(), DecodeOptions::default());
    let seekable = Reader::seekable(Cursor::new(&bytes)).expect("seekable constructs");
    assert_eq!(*seekable.options(), DecodeOptions::default());

    let path = temp_path("shortcut.fits");
    std::fs::write(&path, &bytes).expect("write the fixture");
    let opened = Reader::open(&path);
    std::fs::remove_file(&path).expect("remove the fixture");
    assert_eq!(
        *opened.expect("open constructs").options(),
        DecodeOptions::default()
    );
}
