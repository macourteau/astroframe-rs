//! Deprecations carry a removal version, and this is what enforces it.
//!
//! A deprecation that nobody removes is a second spelling shipped forever, which is the exact
//! outcome the deprecation was meant to end. The note on the item states a version; the reminder
//! to act on it cannot be a person's memory, because the release that should act on it may be
//! months and several maintainers away. So the deadline is a test: it passes while the version is
//! below the removal line and fails the moment it crosses, naming what to delete.
//!
//! **When this fails, that is the test working.** Remove the item it names, then remove its
//! entry here.

/// The crate version as `(major, minor)`, from Cargo's own environment rather than a parse of
/// the manifest — the two cannot disagree.
fn version() -> (u32, u32) {
    (
        env!("CARGO_PKG_VERSION_MAJOR")
            .parse()
            .expect("cargo reports a numeric major version"),
        env!("CARGO_PKG_VERSION_MINOR")
            .parse()
            .expect("cargo reports a numeric minor version"),
    )
}

/// `Reader::open_with_limits`, deprecated in 0.3.1 in favour of `DecodeOptions::open`.
///
/// `DecodeOptions` holds everything a reader is told at construction, so one finisher per source
/// replaces one constructor per source and per knob. Both spellings ship through the 0.3 series;
/// at 0.4 the deprecated one goes.
#[test]
fn reader_open_with_limits_is_removed_at_0_4() {
    assert!(
        version() < (0, 4),
        "version {} has reached the removal line: delete the deprecated \
         `Reader::open_with_limits` (deprecated since 0.3.1), migrate any remaining caller to \
         `DecodeOptions::from(limits).open(path)`, and delete this test",
        env!("CARGO_PKG_VERSION"),
    );
}

/// `Reader::sequential_with_limits`, deprecated in 0.3.1 in favour of `DecodeOptions::sequential`.
///
/// `DecodeOptions` holds everything a reader is told at construction, so one finisher per source
/// replaces one constructor per source and per knob. Both spellings ship through the 0.3 series;
/// at 0.4 the deprecated one goes.
#[test]
fn reader_sequential_with_limits_is_removed_at_0_4() {
    assert!(
        version() < (0, 4),
        "version {} has reached the removal line: delete the deprecated \
         `Reader::sequential_with_limits` (deprecated since 0.3.1), migrate any remaining caller to \
         `DecodeOptions::from(limits).sequential(source)`, and delete this test",
        env!("CARGO_PKG_VERSION"),
    );
}

/// `Reader::seekable_with_limits`, deprecated in 0.3.1 in favour of `DecodeOptions::seekable`.
///
/// `DecodeOptions` holds everything a reader is told at construction, so one finisher per source
/// replaces one constructor per source and per knob. Both spellings ship through the 0.3 series;
/// at 0.4 the deprecated one goes.
#[test]
fn reader_seekable_with_limits_is_removed_at_0_4() {
    assert!(
        version() < (0, 4),
        "version {} has reached the removal line: delete the deprecated \
         `Reader::seekable_with_limits` (deprecated since 0.3.1), migrate any remaining caller to \
         `DecodeOptions::from(limits).seekable(source)`, and delete this test",
        env!("CARGO_PKG_VERSION"),
    );
}
