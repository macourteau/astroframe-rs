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
    /// How an XISF subblock whose compressed and uncompressed lengths are equal is read.
    /// [`EqualLengthSubblocks::Uncompressed`] unless set. FITS has no subblocks and ignores it.
    pub equal_length_subblocks: EqualLengthSubblocks,
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

    /// [`DecodeOptions::equal_length_subblocks`], chained.
    #[must_use]
    pub fn with_equal_length_subblocks(mut self, reading: EqualLengthSubblocks) -> Self {
        self.equal_length_subblocks = reading;
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

/// How an XISF subblock whose compressed and uncompressed lengths are equal is read.
///
/// XISF 1.0 Revision 2 (§10.6.1) settles what such a subblock is: an **uncompressed
/// subblock**, whose stored bytes are its data. An encoder *shall* store a subblock that way
/// whenever compressing it would not make it shorter, and a decoder *shall* copy it
/// unmodified. A block without a `subblocks` attribute is one subblock, whose lengths are the
/// stored size and the declared uncompressed size, so the rule covers it too.
///
/// Earlier revisions said nothing about incompressible subblocks, and an encoder written
/// against them could compress every subblock, leaving some whose compressed length happens to
/// equal the uncompressed one. §10.6.1 lets a decoder read those by trying the codec first.
/// That is [`EqualLengthSubblocks::TryDecompress`], and it is off unless asked for.
///
/// Every other subblock is decompressed under either setting, including one whose compressed
/// length is *larger* than its data, and must decompress to exactly its uncompressed length.
///
/// ```
/// use astroframe::{DecodeOptions, EqualLengthSubblocks};
///
/// let legacy =
///     DecodeOptions::default().with_equal_length_subblocks(EqualLengthSubblocks::TryDecompress);
/// assert_eq!(DecodeOptions::default().equal_length_subblocks, EqualLengthSubblocks::Uncompressed);
/// # let _ = legacy;
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EqualLengthSubblocks {
    /// Copy the stored bytes, as §10.6.1 requires. The default.
    #[default]
    Uncompressed,
    /// Attempt to decompress the subblock first, and use the result only if the codec ends
    /// after producing exactly the subblock's uncompressed length; otherwise copy the stored
    /// bytes. For files written before XISF Revision 2 by an encoder that compressed every
    /// subblock.
    ///
    /// **This is a repair, and it is opt-in for that reason.** A subblock the codec refuses is
    /// read as stored bytes, so a *damaged* compressed subblock of equal lengths decodes to
    /// its compressed bytes as pixels, without an error, unless the block declares a
    /// `checksum` — which is verified over the stored bytes before any of this, and refuses
    /// the damage first. Only the codec rejecting its input falls back: a cap the attempt trips,
    /// such as a zstd frame declaring a window above
    /// [`Limits::zstd_window_bytes`](crate::Limits::zstd_window_bytes), is reported, since the
    /// subblock may be valid compressed data the cap refuses and reading it as pixels would
    /// fabricate them.
    ///
    /// The attempt needs the whole subblock resident, so a `zlib` or `zstd` block that holds
    /// such a subblock reports [`Granularity::Block`](crate::Granularity::Block) rather than
    /// `Rows` — `WholeImage` when it has no `subblocks` attribute — and holds one subblock at
    /// a time, as an LZ4 block does. A block with no equal-length subblock is read exactly as
    /// under the default.
    TryDecompress,
}
