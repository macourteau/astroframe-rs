//! Per-`Reader` decode options. The user-facing prose lives on [`DecodeOptions`] itself, which
//! is where a caller meets it.

use std::path::Path;

use crate::error::Result;
use crate::limits::Limits;
use crate::reader::Reader;
use crate::source::{Seekable, Sequential};

/// Everything a [`Reader`] is told at construction.
///
/// Fixed for a reader's life, like the [`Limits`] it carries and for the same reason: the
/// header parse a constructor performs is already subject to it. [`Reader::options`] reports
/// what is in force.
///
/// Built from [`DecodeOptions::default()`] — `#[non_exhaustive]` so a later option is an
/// addition rather than a break — then chained through the `with_*` setters, and finished by
/// the source it reads, in the manner of [`std::fs::OpenOptions::open`]:
///
/// ```no_run
/// use astroframe::{DecodeOptions, Limits};
///
/// # fn main() -> astroframe::Result<()> {
/// let options =
///     DecodeOptions::default().with_limits(Limits::default().with_total_samples(64 << 20));
///
/// // One value serves a whole batch: the finishers take `&self`.
/// for path in ["a.fits", "b.xisf"] {
///     let mut reader = options.open(path)?;
///     # let _ = reader.next_image()?;
/// }
/// # Ok(()) }
/// ```
///
/// A caller who only moves caps has `From<Limits>`, and [`Reader::open`],
/// [`Reader::sequential`] and [`Reader::seekable`] are these finishers on the defaults:
///
/// ```
/// use astroframe::{DecodeOptions, Limits};
///
/// let limits = Limits::default().with_decoded_output_bytes(4 << 30);
/// assert_eq!(DecodeOptions::from(limits).limits, limits);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// The caps. [`Limits::default()`] unless set.
    pub limits: Limits,
}

impl DecodeOptions {
    /// The defaults, as [`DecodeOptions::default()`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// [`DecodeOptions::limits`], chained.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Open a file, as [`Reader::open`] does, under these options.
    ///
    /// # Errors
    ///
    /// As [`Reader::open`], against these options rather than the defaults.
    pub fn open(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Reader<Seekable<std::io::BufReader<std::fs::File>>>> {
        let file = std::fs::File::open(path)?;
        self.seekable(std::io::BufReader::new(file))
    }

    /// Read a forward-only source, as [`Reader::sequential`] does, under these options.
    ///
    /// # Errors
    ///
    /// As [`Reader::sequential`], against these options rather than the defaults.
    pub fn sequential<R: std::io::Read>(&self, source: R) -> Result<Reader<Sequential<R>>> {
        Reader::build(Sequential::new(source), *self)
    }

    /// Read a seekable source, as [`Reader::seekable`] does, under these options.
    ///
    /// # Errors
    ///
    /// As [`Reader::seekable`], against these options rather than the defaults.
    pub fn seekable<R: std::io::Read + std::io::Seek>(
        &self,
        source: R,
    ) -> Result<Reader<Seekable<R>>> {
        Reader::build(Seekable::new(source)?, *self)
    }
}

impl From<Limits> for DecodeOptions {
    /// The defaults, with `limits` as the caps.
    fn from(limits: Limits) -> Self {
        DecodeOptions::default().with_limits(limits)
    }
}
