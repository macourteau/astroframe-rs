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

/// `Reader::header`, deprecated in 0.2.1 in favour of `Reader::current_header`.
///
/// Its `None` reports a caller error, which `current_header` states as `Error::InvalidRequest`.
/// Both spellings ship through the 0.2 series so a consumer can migrate against a compiler
/// warning rather than a build failure; at 0.3 the deprecated one goes.
#[test]
fn reader_header_is_removed_at_0_3() {
    assert!(
        version() < (0, 3),
        "version {} has reached the removal line: delete the deprecated `Reader::header` \
         (deprecated since 0.2.1), migrate any remaining caller to `Reader::current_header`, \
         and delete this test",
        env!("CARGO_PKG_VERSION"),
    );
}
