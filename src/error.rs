//! One error enum, not per-format errors behind a trait. The user-facing prose lives on
//! [`Error`] itself, which is where a caller meets it.

use crate::header::{DeclineClass, DeclineReason};

/// What went wrong.
///
/// One enum rather than per-format errors behind a trait: what a consumer needs from a
/// failure is a **classification**, not a type hierarchy, and there are three moves it can
/// make.
///
/// | The error says | The caller's move |
/// | --- | --- |
/// | [`Error::Io`] | **Abort the run** — the next frame will probably fail too |
/// | [`Error::Malformed`], [`Error::Unsupported`], [`Error::ChecksumMismatch`], [`Error::LimitExceeded`] | **Skip this frame** and keep walking |
/// | [`Error::InvalidRequest`] | **Fix the call** — this one is the caller's own bug |
///
/// [`Error::is_io`] and [`Error::is_invalid_request`] are the two edges of that table, and a
/// batch loop wants **both**: `if e.is_io() { abort } else { skip }` alone swallows the
/// variant that means the calling program is wrong, and a wrong-sized destination or an
/// out-of-range `select_channel` then reads as "every frame was skipped", with no signal.
/// [`Error::decline_class`] gives the middle row as the same [`DeclineClass`]
/// [`Header::decline_reason`](crate::Header::decline_reason) reports, so a consumer can run
/// one skip path over both.
///
/// Every variant except [`Error::Io`] carries a human-readable reason naming what was
/// expected and what was found, and where in the file when that is known. For a consumer
/// whose documented move on [`Error::Unsupported`] is "skip", the log line *is* the error's
/// value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The source failed. **Abort** — the next frame will probably fail too.
    ///
    /// Truncation is *not* here: a short file is bad data, not a failing disk, so it is
    /// [`Error::Malformed`].
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The bytes are not a valid file of this format — it contradicts itself or the format.
    /// A declared size that disagrees with the geometry, a truncated block, unparseable XML.
    /// Skip this frame.
    #[error("malformed: {0}")]
    Malformed(String),

    /// The file is valid and self-consistent, but uses something this version declines, or
    /// asks something of the *source* it cannot do — an XISF block behind the cursor on a
    /// sequential source is this rather than [`Error::Malformed`], the same file decoding
    /// through `Reader::open`. Skip this frame.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A data block failed verification. Skip this frame.
    #[error("checksum mismatch: {0}")]
    ChecksumMismatch(String),

    /// The file is valid and self-consistent, and tripped a configured cap. Skip this frame.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    /// The **caller** asked for something impossible — a wrong-sized destination slice, a
    /// channel beyond the channel count, a configuration call after the pixel phase began.
    /// Fix the call.
    ///
    /// This variant exists deliberately. Without it the library's only options are a panic —
    /// which the no-panic contract does not cover, that contract being about malformed
    /// *input* — or misreporting a caller bug as a bad file.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

impl Error {
    /// The abort edge of the table on [`Error`]: the source itself failed.
    ///
    /// Not the whole skip-versus-abort split on its own — [`Error::is_invalid_request`] is
    /// the other edge, and everything left over is the skip.
    #[must_use]
    pub fn is_io(&self) -> bool {
        matches!(self, Error::Io(_))
    }

    /// The caller-bug edge of the table on [`Error`]: this program asked for something
    /// impossible.
    ///
    /// A batch loop that skips whatever is not [`Error::is_io`] absorbs this one silently,
    /// which is how a wrong-sized destination becomes "every frame was skipped".
    #[must_use]
    pub fn is_invalid_request(&self) -> bool {
        matches!(self, Error::InvalidRequest(_))
    }

    /// The skippable classification, or `None` for the two variants that are not skippable —
    /// [`Error::Io`], which aborts, and [`Error::InvalidRequest`], which is the caller's bug.
    ///
    /// This is the same taxonomy [`DeclineReason::class`] reports, so a consumer following
    /// the documented flow — check
    /// [`decline_reason()`](crate::Header::decline_reason), else decode — runs **one** "this
    /// position is skippable, and here is why" path over both surfaces rather than two
    /// parallel matches over enums carrying the identical four classes.
    #[must_use]
    pub fn decline_class(&self) -> Option<DeclineClass> {
        match self {
            Error::Malformed(_) => Some(DeclineClass::Malformed),
            Error::Unsupported(_) => Some(DeclineClass::Unsupported),
            Error::LimitExceeded(_) => Some(DeclineClass::LimitExceeded),
            Error::ChecksumMismatch(_) => Some(DeclineClass::ChecksumMismatch),
            Error::Io(_) | Error::InvalidRequest(_) => None,
        }
    }

    pub(crate) fn malformed(reason: impl Into<String>) -> Self {
        Error::Malformed(reason.into())
    }

    pub(crate) fn unsupported(reason: impl Into<String>) -> Self {
        Error::Unsupported(reason.into())
    }

    pub(crate) fn limit(reason: impl Into<String>) -> Self {
        Error::LimitExceeded(reason.into())
    }

    pub(crate) fn invalid_request(reason: impl Into<String>) -> Self {
        Error::InvalidRequest(reason.into())
    }

    // Gated on both features, not on `checksum` alone: the only caller is the XISF block
    // reader, and FITS verifies no digest. `checksum` without `xisf` is a supported member of
    // the powerset -- § Operations makes the whole powerset supported -- and gating on one
    // feature left it warning there.
    #[cfg(all(feature = "checksum", feature = "xisf"))]
    pub(crate) fn checksum(reason: impl Into<String>) -> Self {
        Error::ChecksumMismatch(reason.into())
    }
}

/// The error a pixel call on a declined position raises, built from the report.
///
/// Exposed rather than kept internal because it is the bridge between the two surfaces: a
/// consumer that reports declines and errors through one path builds the error itself instead
/// of writing the four-arm map by hand. The `String` is allocated here, which is once per
/// conversion — [`DeclineReason`]'s own text is shared, and that asymmetry is the reason the
/// two types differ at all.
impl From<&DeclineReason> for Error {
    fn from(decline: &DeclineReason) -> Error {
        match decline.class() {
            DeclineClass::Malformed => Error::Malformed(decline.reason().to_owned()),
            DeclineClass::Unsupported => Error::Unsupported(decline.reason().to_owned()),
            DeclineClass::LimitExceeded => Error::LimitExceeded(decline.reason().to_owned()),
            DeclineClass::ChecksumMismatch => Error::ChecksumMismatch(decline.reason().to_owned()),
        }
    }
}

/// This crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
