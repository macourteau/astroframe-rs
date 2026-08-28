//! `astroframe` reads a FITS or XISF frame and produces two things: the header metadata a
//! tool needs — geometry, exposure, gain, pixel scale, pointing, timestamps — and the pixels
//! themselves, either in the file's own sample type or normalized to `f32` in `[0,1]`,
//! row-major and channel-planar. It is decode-only: nothing in it writes a frame.
//!
//! # Report, don't interpret
//!
//! `astroframe` reports what the file says. It applies exactly one transformation — the
//! sample-to-`[0,1]` normalization the format specifications define — and it applies no
//! other. Policy belongs to the consumer.
//!
//! | Thing the file says | What `astroframe` does |
//! | --- | --- |
//! | FITS `ROWORDER = 'BOTTOM-UP'` | Reports [`RowOrder::BottomUp`]; delivers rows in stored order |
//! | XISF `orientation="90;flip"` | Reports it; delivers samples in stored order |
//! | FITS `PEDESTAL`, XISF `offset` | Reports it; subtracts nothing |
//! | A keyword a consumer cares about | Reports the value as text, verbatim |
//!
//! The rule cuts the other way too: a frame it cannot decode under its documented rules is an
//! error, never a best guess.
//!
//! # Normalization is pinned bit-for-bit
//!
//! One primitive, shared by every container, so the two formats cannot drift apart. Three
//! steps, in this order and no other:
//!
//! ```text
//! k = 1.0f32 / ((hi - lo) as f32)     // the reciprocal, rounded once, computed per range
//! t = (x - lo) as f32                 // the level, offset and narrowed to f32
//! y = t * k                           // the multiply -- never a divide
//! ```
//!
//! It is deliberately **not** the idiomatic `x as f32 / 65535.0`: division is correctly
//! rounded, one rounding, where the pinned form has two, and the two disagree on 512 of
//! 65 536 `UInt16` levels and on 126 of 256 `UInt8` levels. Which is *more* accurate is not
//! the point — the point is that one of them has to be chosen and written down, because a
//! consumer comparing this crate's output against another decoder's needs to know which.
//! See [`Normalizer`] and `docs/intentional-patterns.md`.
//!
//! **The decoded bits are part of the public API.** A release that changes one ULP of output
//! for an input it previously decoded is a breaking change.
//!
//! # Three tiers
//!
//! ```no_run
//! use astroframe::Reader;
//!
//! # fn main() -> astroframe::Result<()> {
//! let mut reader = Reader::open("frame.fits")?;
//! while reader.next_image()? {
//!     let header = reader.current_header()?;
//!     if header.decline_reason().is_some() {
//!         continue; // a position this version will not decode
//!     }
//!     let image = reader.read_image()?;
//!     println!("{}x{}", image.width(), image.height());
//! }
//! # Ok(()) }
//! ```
//!
//! Tier 1 is the header alone — constructing a `Reader` reads no pixel byte. Tier 2 decodes a
//! whole image into a destination. Tier 3 is chunked delivery, and tier 2 is implemented on
//! top of it, which makes "streamed and whole-buffer decode produce bit-identical buffers"
//! true by construction rather than by two code paths agreeing — and a tier-3 caller reaches
//! the same guarantee by handing [`Reader::normalizer`] to [`Chunk::normalize_into`].
//!
//! # Streaming is real, and the API says how real before you decode
//!
//! [`Header::granularity`] reports **how much of the input the decoder must hold before it
//! can produce any sample** — not how large a chunk you receive, and not where in the
//! destination those samples land. Conflating any of the three is the easy mistake.
//!
//! | Source | Granularity | Peak memory above the destination buffer |
//! | --- | --- | --- |
//! | FITS, any `BITPIX` | `Rows` | one I/O buffer (kilobytes) |
//! | XISF, uncompressed, no checksum | `Rows` | one I/O buffer |
//! | XISF, `zlib` or `zstd`, no shuffling, no checksum | `Rows` | I/O buffer + the codec's window |
//! | XISF, `lz4`/`lz4hc` **+ `subblocks`**, no shuffling, no checksum | `Block` | one subblock, compressed and decompressed |
//! | XISF, `zlib` **+ `subblocks`**, no shuffling, no checksum | `Rows` | zlib's floor is already `Rows`; `subblocks` cannot lower it |
//! | XISF, `subblocks` **+ shuffling** | `WholeImage` | the shuffle spans the whole pre-split block |
//! | XISF, `subblocks` **+ checksum** | `WholeImage` | the digest covers the whole stored block |
//! | XISF, `embedded` | `WholeImage` | the pixels were materialized during the header parse |
//! | XISF, one `lz4`/`lz4hc` or checksummed block covering the image | `WholeImage` | the stored block plus its decompressed form |
//!
//! Each property of a block imposes a **floor**, and the granularity is the **worst** of
//! them, not the first one found. A `Block` floor is promoted to `WholeImage` unless the
//! block is split into subblocks; a `Rows` floor is never promoted.
//!
//! What that is worth, honestly: tier 1 is the big win and applies to every format and codec
//! — a metadata sweep over a night's frames stops being a decode at all. FITS tier 2 is worth
//! roughly half of peak. Real integrated masters are uncompressed and unchecksummed, so XISF
//! streams by rows exactly as FITS does. For a compressed single-block frame streaming wins
//! nothing, and the API says so before you decode.
//!
//! # Hardening
//!
//! Malformed input produces an `Err`, never a panic, a hang, or unbounded allocation. Every
//! size the header declares is a cross-check rather than an allocation size, and [`Limits`]
//! bounds the geometry that is not. The crate sets `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
// The README's example is compiled and run as a doctest, so a snippet that drifts from the API
// fails the build rather than misleading a reader. `cfg(doctest)` is what keeps the README out
// of the rendered crate documentation, which covers the same ground in its own words.
#![cfg_attr(doctest, doc = include_str!("../README.md"))]

// Every public item has exactly one stable path: the modules are private and the root
// re-exports them. Publishing both `astroframe::Error` and `astroframe::error::Error` would
// document each type twice and semver-lock nine module paths for nothing — a caller writes
// one `use astroframe::{…}` either way. What the modules keep is their `//!` prose, which is
// maintainer-facing; the part a caller needs lives on the types, where it is met.
pub(crate) mod error;
pub(crate) mod header;
pub(crate) mod image;
pub(crate) mod limits;
pub(crate) mod metadata;
pub(crate) mod normalize;
pub(crate) mod reader;
pub(crate) mod samples;
pub(crate) mod source;

#[cfg(feature = "fits")]
pub(crate) mod fits;
#[cfg(feature = "xisf")]
pub(crate) mod xisf;

pub use error::{Error, Result};
pub use header::{
    Bounds, BoundsUnavailable, ColorSpace, DeclineClass, DeclineReason, Format, Geometry,
    Granularity, Header, ImageType, Orientation, PixelStorage, RowOrder,
};
pub use image::Image;
pub use limits::Limits;
pub use metadata::{
    Cfa, DisplayChannels, DisplayFunction, Keyword, KeywordIter, KeywordOrigin, Keywords,
    Properties, Property, PropertyIter, PropertyScope, PropertyType, PropertyValue, Resolution,
    ResolutionUnit, ValueKind,
};
pub use normalize::{Normalizer, Sample, SampleRange, Scaling};
pub use reader::{Chunk, Chunks, Reader};
pub use samples::{IterF64, SampleFormat, SampleSlice, Samples};
pub use source::{Seekable, Sequential, Source};
