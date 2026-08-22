//! One error enum, not per-format errors behind a trait.
//!
//! The consumer's real need is a two-way split — *skip this frame* versus *abort the run* —
//! and that is a classification rather than a type hierarchy.

/// What went wrong.
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
    /// The skip-versus-abort split, in one call.
    pub fn is_io(&self) -> bool {
        matches!(self, Error::Io(_))
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

/// This crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
