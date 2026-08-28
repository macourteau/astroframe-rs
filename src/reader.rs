//! The three-tier API. The user-facing prose lives on [`Reader`] itself, which is where a
//! caller meets it.

use std::ops::{ControlFlow, Range};
use std::path::Path;

use crate::error::{Error, Result};
use crate::header::{Bounds, BoundsUnavailable, Format, Header};
use crate::image::Image;
use crate::limits::{Limits, narrow};
use crate::normalize::{Normalizer, SampleRange};
use crate::samples::{SampleSlice, Samples};
use crate::source::{Seekable, Sequential, Source};

/// One contiguous run of one channel's samples, in native form.
///
/// It borrows the reader's scratch buffer rather than owning anything, which is why the
/// cursor is a `while let` loop rather than an `Iterator`.
///
/// Chunk extent is the reader's choice and is independent of
/// [`Granularity`](crate::Granularity) — a `WholeImage` source still delivers chunks, it
/// simply had to read everything before the first one.
#[derive(Debug)]
pub struct Chunk<'a> {
    channel: u32,
    range: Range<usize>,
    samples: SampleSlice<'a>,
}

impl<'a> Chunk<'a> {
    /// The **file's** channel index, never renumbered to zero under `select_channel`.
    ///
    /// [`Image::channel`] runs the other way, indexing the image's own channels. The two
    /// numbering schemes are deliberate and neither is derivable from the other.
    pub fn channel(&self) -> u32 {
        self.channel
    }

    /// The sample range this chunk covers, in **destination coordinates** — offsets into the
    /// buffer the caller supplied.
    ///
    /// So assembling a buffer from chunks is a copy at the stated offset with no
    /// recalculation. The distinction only bites under `select_channel`, where file and
    /// destination coordinates diverge: the range is always the destination's, the channel
    /// index always the file's.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// The samples themselves, in the file's own type.
    pub fn samples(&self) -> SampleSlice<'a> {
        self.samples
    }

    /// Normalize this chunk into its slice of a destination, with the primitive
    /// [`Reader::normalizer`] built for the image.
    ///
    /// This is the whole of what tier 2 does per chunk — [`Reader::read_image_into`] calls
    /// exactly this — so a tier-3 caller assembling normalized `f32` gets bits identical to a
    /// whole-buffer decode by construction rather than by reimplementing the nine-arm sample
    /// dispatch and hoping it matches.
    ///
    /// `dst` is the destination's [`Chunk::range`], which is already in destination
    /// coordinates: `chunk.normalize_into(&n, &mut buffer[chunk.range()])`.
    ///
    /// # Panics
    ///
    /// If `dst` is not exactly this chunk's sample count, like
    /// [`Normalizer::normalize_into`] and [`slice::copy_from_slice`]. That is a programmer
    /// error rather than a decode outcome; the no-panic contract is about malformed *input*.
    pub fn normalize_into(&self, normalizer: &Normalizer, dst: &mut [f32]) {
        match self.samples {
            SampleSlice::U8(s) => normalizer.normalize_into(s, dst),
            SampleSlice::U16(s) => normalizer.normalize_into(s, dst),
            SampleSlice::U32(s) => normalizer.normalize_into(s, dst),
            SampleSlice::U64(s) => normalizer.normalize_into(s, dst),
            SampleSlice::I16(s) => normalizer.normalize_into(s, dst),
            SampleSlice::I32(s) => normalizer.normalize_into(s, dst),
            SampleSlice::I64(s) => normalizer.normalize_into(s, dst),
            SampleSlice::F32(s) => normalizer.normalize_into(s, dst),
            SampleSlice::F64(s) => normalizer.normalize_into(s, dst),
        }
    }
}

/// The pull form of chunked delivery.
///
/// Constructing one commits the reader to the pixel phase; every error the phase raises
/// surfaces from [`Chunks::next_chunk`].
#[derive(Debug)]
#[must_use = "constructing a cursor commits the reader to the pixel phase, so dropping one \
              unused forbids set_bounds and select_channel while delivering nothing"]
pub struct Chunks<'a, S: Source> {
    reader: &'a mut Reader<S>,
}

impl<S: Source> Chunks<'_, S> {
    /// The next chunk, or `None` at the end of the image.
    ///
    /// A chunk borrows the reader's scratch buffer, so this is a
    /// `while let Some(chunk) = chunks.next_chunk()?` loop rather than an `Iterator` — the
    /// idiomatic Rust answer for lending iteration.
    ///
    /// # Errors
    ///
    /// Every error the pixel phase raises surfaces here, the cursor's own construction being
    /// infallible: [`Error::Io`] from the source, [`Error::Malformed`],
    /// [`Error::Unsupported`], [`Error::ChecksumMismatch`] or [`Error::LimitExceeded`] from
    /// the position, and the [`Error::InvalidRequest`] that says the image's decode already
    /// failed — a pixel-phase error poisons the image, and pulling again after being told so
    /// is the caller's mistake. Advance to the next image, or start a fresh cursor to retry
    /// this one, which [`Reader::is_seekable`] says whether the source allows.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk<'_>>> {
        match self.reader.advance_chunk()? {
            None => Ok(None),
            Some(meta) => {
                let samples = self.reader.scratch_slice(meta.len);
                // `checked_add`, for the reason `output_bytes` is written checked: the
                // total-samples gate already rules out a destination whose extent does not
                // fit, so this cannot fail today — but § The caps makes checked arithmetic
                // the rule for every size computation, and that gate's guarantee is an
                // argument in another function rather than a fact local to this line.
                let end = meta.start.checked_add(meta.len).ok_or_else(|| {
                    Error::limit(format!(
                        "chunk destination range: a chunk of {} samples at offset {} ends past \
                         the arithmetic a destination index runs in",
                        meta.len, meta.start
                    ))
                })?;
                Ok(Some(Chunk {
                    channel: meta.channel,
                    range: meta.start..end,
                    samples,
                }))
            }
        }
    }
}

/// Where a chunk's samples land and which channel they belong to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChunkMeta {
    /// The file's channel index.
    pub(crate) channel: u32,
    /// Offset into the caller's destination buffer.
    pub(crate) start: usize,
    /// How many samples.
    pub(crate) len: usize,
}

/// What the reader was configured to produce, handed to a format decoder when the pixel
/// phase begins.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PixelPlan {
    /// The file channel the caller narrowed to, if any.
    ///
    /// With both format features disabled no decoder ever reads this (see `dispatch!` below),
    /// so it is dead code in that configuration specifically, not in general.
    #[cfg_attr(not(any(feature = "fits", feature = "xisf")), allow(dead_code))]
    pub(crate) selected_channel: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Pixel,
}

enum Inner {
    #[cfg(feature = "fits")]
    Fits(Box<crate::fits::Decoder>),
    #[cfg(feature = "xisf")]
    Xisf(Box<crate::xisf::Decoder>),
    // `Reader::build` never produces one — `sniff` always returns `Err(Unsupported)` first
    // (see `dispatch!` below) — but the variant still has to exist for `Inner` to be a type at
    // all, so the compiler's dead-code check is right and is silenced deliberately.
    #[cfg(not(any(feature = "fits", feature = "xisf")))]
    #[allow(dead_code)]
    NoFormat,
}

/// Dispatch one method across whichever format decoders are compiled in.
///
/// The result type is spelled at every call site rather than inferred. That is not
/// ceremony: with **both** format features disabled the match has no arms but the
/// no-format one, whose body diverges, so there is nothing for the compiler to infer from
/// — and that configuration is a documented, supported one, in which the crate still
/// exposes `Header`, `Samples` and the normalization primitive and every constructor
/// returns `Unsupported`.
macro_rules! dispatch {
    ($inner:expr, $d:ident => $body:expr, $ty:ty) => {{
        let out: $ty = match $inner {
            #[cfg(feature = "fits")]
            Inner::Fits($d) => $body,
            #[cfg(feature = "xisf")]
            Inner::Xisf($d) => $body,
            #[cfg(not(any(feature = "fits", feature = "xisf")))]
            Inner::NoFormat => {
                unreachable!("no format is compiled in, so no Reader was ever constructed")
            }
        };
        // With both format features disabled, `NoFormat` is the only arm and it diverges, so
        // `out` is unreachable to the compiler at every one of this macro's ~34 call sites.
        // That configuration is supported (§ Operations), so the warning is silenced once
        // here rather than at each site.
        #[allow(unreachable_code)]
        out
    }};
}

/// Reads a FITS or XISF source, one image at a time.
///
/// **Three tiers**, and "tier" is about how much of the file a caller asks for; it is
/// unrelated to the two *layers*, which are about what the bytes have been turned into.
///
/// - **Tier 1 — header only.** Constructing a `Reader` parses the first header unit and
///   stops; no pixel byte is read. A tool sweeping a night's frames for pixel scale and
///   timestamps pays only this.
/// - **Tier 2 — whole-image decode into a destination.** [`Reader::read_image_into`] fills a
///   caller-owned buffer, so a batch consumer allocates once and reuses across frames.
/// - **Tier 3 — chunked delivery.** [`Reader::chunks`] is the pull form,
///   [`Reader::for_each_chunk`] the push form implemented by driving it. A caller wanting
///   normalized `f32` out of tier 3 asks for [`Reader::normalizer`] once and hands it to
///   [`Chunk::normalize_into`] per chunk — the same primitive tier 2 runs.
///
/// **Tier 2 is implemented on top of tier 3**, which makes "streamed and whole-buffer decode
/// produce bit-identical buffers" true by construction rather than by two code paths
/// agreeing. No separate optimized whole-image path may be added later: invariant I2 would
/// then rest on a test rather than on structure.
///
/// `Send` when its source is, and `Sync` when its source is — the auto traits apply, since
/// nothing here has interior mutability. Every useful method takes `&mut self`, so a shared
/// `&Reader` is sound but buys nothing; that is a statement about the API, not about `Sync`.
pub struct Reader<S: Source> {
    source: S,
    limits: Limits,
    inner: Inner,
    phase: Phase,
    /// Set by `set_bounds`; per-image, cleared by `next_image`.
    ///
    /// The validated [`SampleRange`], not the pair the caller wrote: `set_bounds` has already
    /// applied the validity rule, and keeping the pair would mean re-deriving `k` at
    /// normalize time from numbers that have already passed.
    bounds_override: Option<SampleRange>,
    /// Set by `select_channel`; per-image, cleared by `next_image`.
    selected_channel: Option<u32>,
    /// `next_image()` advances, against `Limits::images_per_source`.
    ///
    /// `u64` rather than `u32`, so a `u32::MAX` cap stays comparable: `saturating_add(1)` on a
    /// `u32` counter would itself saturate at `u32::MAX`, and the cap would then never trip no
    /// matter how far past it the source runs.
    advances: u64,
    /// Whether the current image's pixel phase has been started.
    pixels_started: bool,
    /// Set when a pixel-phase call has returned an error. See [`Reader::advance_chunk`].
    pixels_failed: bool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            #[cfg(feature = "fits")]
            Inner::Fits(_) => "Fits",
            #[cfg(feature = "xisf")]
            Inner::Xisf(_) => "Xisf",
            #[cfg(not(any(feature = "fits", feature = "xisf")))]
            Inner::NoFormat => "NoFormat",
        };
        f.write_str(name)
    }
}

impl<S: Source + std::fmt::Debug> std::fmt::Debug for Reader<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("format", &self.inner)
            .field("phase", &self.phase)
            .field("advances", &self.advances)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------- constructors

impl Reader<Seekable<std::io::BufReader<std::fs::File>>> {
    /// Open a file. Seekable, so block order does not matter and the source length is known.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the file cannot be opened, and whatever the first header unit's parse
    /// raises: [`Error::Malformed`] for bytes matching neither format's signature,
    /// [`Error::Unsupported`] for a format this build has disabled, [`Error::LimitExceeded`]
    /// for a header past one of [`Limits`]' caps.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_limits(path, Limits::default())
    }

    /// [`Reader::open`] with the caller's caps.
    ///
    /// # Errors
    ///
    /// As [`Reader::open`], against `limits` rather than the defaults.
    pub fn open_with_limits(path: impl AsRef<Path>, limits: Limits) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        Reader::seekable_with_limits(std::io::BufReader::new(file), limits)
    }
}

impl<R: std::io::Read> Reader<Sequential<R>> {
    /// Read forward-only — a pipe, a socket, a decompressed stream.
    ///
    /// Skipping is read-and-discard, so a block behind the cursor is [`Error::Unsupported`]
    /// rather than a silent buffer, and there is no source length for the geometry check to
    /// use.
    ///
    /// **Buffer the source yourself.** Only [`Reader::open`] wraps what it is given in a
    /// [`std::io::BufReader`]; this takes the reader verbatim, and the decoder issues many
    /// small reads — one per 2880-byte FITS header block, one per pixel row — each of which
    /// is a syscall on a bare pipe or socket.
    ///
    /// # Errors
    ///
    /// Whatever the first header unit's parse raises — [`Error::Io`] from the source itself,
    /// [`Error::Malformed`] for bytes matching neither format's signature,
    /// [`Error::Unsupported`] for a format this build has disabled, [`Error::LimitExceeded`]
    /// for a header past one of [`Limits`]' caps.
    pub fn sequential(source: R) -> Result<Self> {
        Self::sequential_with_limits(source, Limits::default())
    }

    /// [`Reader::sequential`] with the caller's caps.
    ///
    /// # Errors
    ///
    /// As [`Reader::sequential`], against `limits` rather than the defaults.
    pub fn sequential_with_limits(source: R, limits: Limits) -> Result<Self> {
        Reader::build(Sequential::new(source), limits)
    }
}

impl<R: std::io::Read + std::io::Seek> Reader<Seekable<R>> {
    /// Read a seekable source.
    ///
    /// Decoding from an **in-memory buffer** is `Reader::seekable(Cursor::new(bytes))`; no
    /// separate constructor exists, because a `Cursor` already is a seekable source and a
    /// third entry point would only be an alias.
    ///
    /// **Buffer a source that is not already in memory.** Only [`Reader::open`] wraps what it
    /// is given in a [`std::io::BufReader`]; a bare [`std::fs::File`] handed here pays one
    /// syscall per 2880-byte FITS header block and one per pixel row. A `Cursor` needs none.
    ///
    /// # Errors
    ///
    /// As [`Reader::sequential`], plus the [`Error::Io`] a source that cannot report its own
    /// length raises — the constructor measures it, which is what the geometry cross-check
    /// uses.
    pub fn seekable(source: R) -> Result<Self> {
        Self::seekable_with_limits(source, Limits::default())
    }

    /// [`Reader::seekable`] with the caller's caps.
    ///
    /// # Errors
    ///
    /// As [`Reader::seekable`], against `limits` rather than the defaults.
    pub fn seekable_with_limits(source: R, limits: Limits) -> Result<Self> {
        Reader::build(Seekable::new(source)?, limits)
    }
}

// With both format features disabled, `Reader::build` (below) always returns `Err` before a
// `Reader` exists, so every method in this block is unreachable in that configuration and the
// locals that only feed a `dispatch!` call — the reader's own arguments to the decoder it
// dispatches to — read as unused. That configuration is supported (§ Operations), so the lint
// is silenced for it specifically rather than papering over a real unused binding elsewhere.
#[cfg_attr(not(any(feature = "fits", feature = "xisf")), allow(unused_variables))]
impl<S: Source> Reader<S> {
    fn build(mut source: S, limits: Limits) -> Result<Self> {
        let mut signature = [0u8; 8];
        source.read_exact(&mut signature).map_err(|e| match e {
            Error::Malformed(_) => {
                Error::malformed("source is shorter than the 8 bytes needed to identify a format")
            }
            other => other,
        })?;

        let inner = Self::sniff(&signature, &mut source, &limits)?;

        Ok(Reader {
            source,
            limits,
            inner,
            phase: Phase::Header,
            bounds_override: None,
            selected_channel: None,
            advances: 0,
            pixels_started: false,
            pixels_failed: false,
        })
    }

    #[allow(unused_variables)]
    fn sniff(signature: &[u8; 8], source: &mut S, limits: &Limits) -> Result<Inner> {
        if signature.starts_with(b"SIMPLE") {
            #[cfg(feature = "fits")]
            {
                return Ok(Inner::Fits(Box::new(crate::fits::Decoder::new(
                    signature, source, limits,
                )?)));
            }
            #[cfg(not(feature = "fits"))]
            return Err(Error::unsupported(
                "this build has the `fits` feature disabled",
            ));
        }
        if signature.starts_with(b"XISF") {
            if signature != b"XISF0100" {
                return Err(Error::unsupported(format!(
                    "XISF signature {:?}: this version reads XISF 1.0 only",
                    String::from_utf8_lossy(signature)
                )));
            }
            #[cfg(feature = "xisf")]
            {
                return Ok(Inner::Xisf(Box::new(crate::xisf::Decoder::new(
                    source, limits,
                )?)));
            }
            #[cfg(not(feature = "xisf"))]
            return Err(Error::unsupported(
                "this build has the `xisf` feature disabled",
            ));
        }
        Err(Error::malformed(format!(
            "leading bytes {:?} match neither the FITS `SIMPLE` card nor the XISF `XISF0100` \
             signature",
            String::from_utf8_lossy(signature)
        )))
    }

    /// The caps this reader was built with.
    ///
    /// They are fixed for its life, so this reports what governed the header parse the
    /// constructor already performed as well as what will govern the decode — which is what a
    /// caller reporting a [`Error::LimitExceeded`] needs in order to name the cap it tripped.
    ///
    /// ```no_run
    /// use astroframe::{Limits, Reader};
    ///
    /// # fn main() -> astroframe::Result<()> {
    /// let limits = Limits::default().with_total_samples(64 << 20);
    /// let reader = Reader::open_with_limits("frame.fits", limits)?;
    /// assert_eq!(reader.limits().total_samples, 64 << 20);
    /// # Ok(()) }
    /// ```
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Which container this reader is decoding.
    ///
    /// Answered from construction, before the first [`Reader::next_image`] — the constructor
    /// sniffed the leading bytes, so there is no position to advance to first, and a caller
    /// choosing caps or a code path per format asks here rather than decoding one image to
    /// find out. [`Header::format`] reports the same fact per position, which is the shape a
    /// caller holding only a `Header` needs.
    ///
    /// ```no_run
    /// use astroframe::{Format, Reader};
    ///
    /// # fn main() -> astroframe::Result<()> {
    /// let reader = Reader::open("frame.fits")?;
    /// assert_eq!(reader.format(), Format::Fits);
    /// # Ok(()) }
    /// ```
    pub fn format(&self) -> Format {
        match &self.inner {
            #[cfg(feature = "fits")]
            Inner::Fits(_) => Format::Fits,
            #[cfg(feature = "xisf")]
            Inner::Xisf(_) => Format::Xisf,
            #[cfg(not(any(feature = "fits", feature = "xisf")))]
            Inner::NoFormat => {
                unreachable!("no format is compiled in, so no Reader was ever constructed")
            }
        }
    }

    /// Whether this reader's source can move its cursor backwards.
    ///
    /// [`Source`] is a bare marker and stays one — its operations are the decoders' interface
    /// to the bytes, not a caller's. But seekability is a fact the *caller's* moves depend on,
    /// and the documented moves say so: re-decoding an image by starting a fresh
    /// [`Reader::chunks`] cursor works on a seekable source and is [`Error::Unsupported`] on a
    /// sequential one, as is an XISF block lying behind the cursor. Generic code written to
    /// the documented bound `fn run<S: Source>(reader: &mut Reader<S>)` has no other way to
    /// ask, and would otherwise have to be told by whoever constructed the reader.
    ///
    /// ```no_run
    /// # fn f(reader: &mut astroframe::Reader<impl astroframe::Source>) -> astroframe::Result<()> {
    /// let mut buffer = vec![0.0f32; reader.destination_len()?];
    /// reader.read_image_into(&mut buffer)?;
    /// if reader.is_seekable() {
    ///     // Decoding the same image a second time needs the cursor to go back.
    ///     reader.read_image_into(&mut buffer)?;
    /// }
    /// # Ok(()) }
    /// ```
    pub fn is_seekable(&self) -> bool {
        self.source.is_seekable()
    }

    // ------------------------------------------------------------ header phase

    /// The current image's header, or `None` until the first successful
    /// [`Reader::next_image`].
    ///
    /// Owned rather than borrowed: a borrow would hold the reader immutably while every
    /// pixel-phase method needs `&mut self`.
    ///
    /// **Fetch it after configuring the reader.** `select_channel` narrows the reported
    /// channel count, so a header taken *before* that call still describes the file's full
    /// channel count and would size a buffer the narrowed reader then rejects.
    ///
    /// [`Reader::current_header`] is the same value past the first advance, as a `Result`
    /// rather than an `Option` — which is what a caller inside the `while next_image()?` loop
    /// wants, the `None` being unreachable there.
    pub fn header(&self) -> Option<Header> {
        let base = dispatch!(&self.inner, d => d.header(), Option<&Header>)?;
        let mut header = base.clone();

        if let Some(k) = self.selected_channel {
            if let Some(g) = header.geometry.as_mut() {
                g.channels = 1;
            }
            header.channel_index = Some(k);
        }

        if let Some(effective) = self.bounds_override {
            let declared = match &base.bounds {
                Bounds::CallerSupplied { declared, .. } => declared.clone(),
                // Both arms need the file's own text, not a re-rendered numeric pair: a
                // `Declared` bounds parsed successfully but must still report what the file
                // wrote (`1.500e+03`, not `1500`), and an `InvalidDeclared` one never parsed
                // at all.
                Bounds::Declared(..) | Bounds::Unavailable(BoundsUnavailable::InvalidDeclared) => {
                    dispatch!(&self.inner, d => d.declared_bounds_text(), Option<String>)
                }
                _ => None,
            };
            header.bounds = Bounds::CallerSupplied {
                effective,
                declared,
            };
        }

        Some(header)
    }

    /// The current image's header, or the [`Error::InvalidRequest`] that says no image is
    /// selected.
    ///
    /// The same value [`Reader::header`] reports, in the shape the documented loop wants:
    /// inside `while reader.next_image()? { … }` a header always exists, and the `Option`
    /// there is a fact about the reader before the first advance rather than about the
    /// position.
    ///
    /// It reports a **declined** position like any other — the decline is on
    /// [`Header::decline_reason`], which is where a walk checks it. Only a pixel call turns
    /// that into an error.
    ///
    /// **Fetch it after configuring the reader**, exactly as [`Reader::header`] describes.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`], and only that: calling before the first
    /// [`Reader::next_image`], or after the one that reported end of source, is the caller's
    /// mistake and is reported as one.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn f(reader: &mut astroframe::Reader<impl astroframe::Source>) -> astroframe::Result<()> {
    /// while reader.next_image()? {
    ///     let header = reader.current_header()?;
    ///     if header.decline_reason().is_some() {
    ///         continue;
    ///     }
    /// }
    /// # Ok(()) }
    /// ```
    pub fn current_header(&self) -> Result<Header> {
        self.header()
            .ok_or_else(|| Error::invalid_request("no image is selected; call next_image first"))
    }

    /// Advance to the next image, uniformly across both formats and every file layout.
    ///
    /// Construction selects no image, so a single-image source returns `true` then `false`.
    /// End of source is `Ok(false)`, not an error.
    ///
    /// ```no_run
    /// # fn f(reader: &mut astroframe::Reader<impl astroframe::Source>) -> astroframe::Result<()> {
    /// while reader.next_image()? {
    ///     // …decode the current image…
    /// }
    /// # Ok(()) }
    /// ```
    ///
    /// The images-per-source cap counts **advances**, not HDUs, so a FITS file with three
    /// hundred tables between two images is nowhere near it.
    ///
    /// Reader state is per-image: `set_bounds` and `select_channel` are cleared here. The
    /// alternative silently carries a `Float32` image's bounds onto the `UInt16` image after
    /// it, which a multi-image XISF file makes reachable.
    ///
    /// Always legal after an early stop: whatever remains of an abandoned image's data is
    /// skipped first.
    ///
    /// # Errors
    ///
    /// End of source is `Ok(false)` rather than an error. [`Error::LimitExceeded`] when the
    /// source holds more image occurrences than [`Limits::images_per_source`] allows, and
    /// [`Error::Io`], [`Error::Malformed`] or [`Error::Unsupported`] from reading the next
    /// header unit — including the skip past an abandoned image's remaining data, which a
    /// sequential source performs by reading.
    pub fn next_image(&mut self) -> Result<bool> {
        if self.pixels_started {
            let src = &mut self.source;
            let limits = &self.limits;
            dispatch!(&mut self.inner, d => d.abandon(src, limits), Result<()>)?;
        }

        self.phase = Phase::Header;
        self.bounds_override = None;
        self.selected_channel = None;
        self.pixels_started = false;
        self.pixels_failed = false;

        let src = &mut self.source;
        let limits = &self.limits;
        let advanced = dispatch!(&mut self.inner, d => d.next_image(src, limits), Result<bool>)?;

        // The cap counts **occurrences**, so it is checked only once the decoder confirms
        // there is another one. Counting the advance first makes the terminating call -- the
        // one that reports end of source -- count against the cap, so a file holding exactly
        // the cap cannot be walked to its end. § The caps says *more than* the cap is
        // `LimitExceeded`, and a 256-image mosaic is not more than 256.
        if advanced {
            self.advances = self.advances.saturating_add(1);
            if self.advances > u64::from(self.limits.images_per_source) {
                return Err(Error::limit(format!(
                    "images per source: this source holds more than the {} image occurrences \
                     the cap allows",
                    self.limits.images_per_source
                )));
            }
        }
        Ok(advanced)
    }

    // ----------------------------------------------------------- configuration

    /// Override the representable range the normalized output maps against.
    ///
    /// A setter, and named as one: [`Reader::select_channel`] is its sibling, both being
    /// imperative configuration calls that take `&mut self` and can fail. The `with_` prefix
    /// belongs to [`Limits`]' builder methods, which consume and return `Self` so a caller can
    /// chain them; one prefix carrying two contracts is what makes
    /// `reader.set_bounds(..).select_channel(..)` look chainable when it is not.
    ///
    /// **Its operands are physical values** — post-`BSCALE`/`BZERO`, the units the range map
    /// works in. So on a `BITPIX = 16`, `BZERO = 32768` frame the pair that reproduces the
    /// default range is `(0, 65535)`, and `(-32768, 32767)` yields a different image rather
    /// than the same one written another way.
    ///
    /// Refuses the same values a file-declared `bounds` is refused for — the range validity
    /// rule in [`SampleRange::new`] — but as [`Error::InvalidRequest`] rather than
    /// [`Error::Malformed`], the caller being the one at fault.
    ///
    /// May be called again before the pixel phase begins, and the **last call wins**; a
    /// second call is not an error. Each call is validated on its own, so a rejected second
    /// call leaves the first in force rather than clearing the range.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`], and only that — for a pair failing the range validity rule,
    /// and for a call after the pixel phase has begun, from which configuration is fixed.
    pub fn set_bounds(&mut self, lo: f64, hi: f64) -> Result<()> {
        self.require_header_phase("set_bounds")?;
        let Some(range) = SampleRange::new(lo, hi) else {
            return Err(Error::invalid_request(format!(
                "set_bounds({lo}, {hi}): 1.0f32 / ((hi - lo) as f32) must be finite, positive \
                 and normal"
            )));
        };
        self.bounds_override = Some(range);
        Ok(())
    }

    /// Narrow the reader to one channel of the file.
    ///
    /// The reported channel count becomes `1` and the expected destination length becomes
    /// `width * height`, so a caller sizing a buffer from the header is right by
    /// construction — **provided it fetches the header after this call**.
    ///
    /// May be called again before the pixel phase begins, and the last call wins; it narrows
    /// from the *file's* channels each time, not from the previous narrowing.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`], and only that — for a `k` at or beyond the file's channel
    /// count, for a position whose channel count is `None`, there being no count for `k` to be
    /// within, and for a call after the pixel phase has begun.
    pub fn select_channel(&mut self, k: u32) -> Result<()> {
        self.require_header_phase("select_channel")?;
        let channels = dispatch!(&self.inner, d => d.header(), Option<&Header>)
            .and_then(|h| h.channels())
            .ok_or_else(|| {
                Error::invalid_request(format!(
                    "select_channel({k}): this position reports no channel count"
                ))
            })?;
        if k >= channels {
            return Err(Error::invalid_request(format!(
                "select_channel({k}): the file declares {channels} channels"
            )));
        }
        self.selected_channel = Some(k);
        Ok(())
    }

    fn require_header_phase(&self, what: &str) -> Result<()> {
        if self.phase == Phase::Pixel {
            return Err(Error::invalid_request(format!(
                "{what}: the pixel phase has begun; configuration is fixed from that point"
            )));
        }
        Ok(())
    }

    // -------------------------------------------------------------- pixel phase

    /// Chunked delivery, pull form.
    ///
    /// Constructing the cursor is enough to commit the reader to the pixel phase — the
    /// boundary is not deferred to the first [`Chunks::next_chunk`], because a caller holding
    /// a cursor has already committed the reader. Infallible: every error the phase raises
    /// surfaces from `next_chunk`.
    ///
    /// Dropping the cursor ends delivery without error, leaving the reader positioned
    /// mid-image; [`Reader::next_image`] is still legal and skips whatever remains.
    #[must_use = "constructing a cursor commits the reader to the pixel phase, so dropping one \
                  unused forbids set_bounds and select_channel while delivering nothing"]
    pub fn chunks(&mut self) -> Chunks<'_, S> {
        self.phase = Phase::Pixel;
        // A new cursor is a new stream over the current image, so the next `next_chunk` runs
        // the pixel-phase order again from the top. Re-reading the same image is legal only
        // on a seekable source; on a sequential one the rewind is `Unsupported`, which is
        // where that rule is enforced rather than here.
        self.pixels_started = false;
        self.pixels_failed = false;
        Chunks { reader: self }
    }

    /// Chunked delivery, push form — implemented by driving [`Reader::chunks`].
    ///
    /// The callback returns [`ControlFlow`], so a caller can stop early without inventing an
    /// error. A callback needing to fail with its own error keeps it in captured state and
    /// returns `Break`; the break carries no payload for exactly that reason.
    ///
    /// # Errors
    ///
    /// Whatever [`Chunks::next_chunk`] raises, this being that loop written out. A callback
    /// returning `Break` is not an error.
    pub fn for_each_chunk<F>(&mut self, mut f: F) -> Result<()>
    where
        F: FnMut(&Chunk<'_>) -> ControlFlow<()>,
    {
        let mut chunks = self.chunks();
        while let Some(chunk) = chunks.next_chunk()? {
            if f(&chunk).is_break() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Decode native samples into a caller-owned buffer.
    ///
    /// The `Samples` variant must match the header's sample format — a mismatch is
    /// [`Error::InvalidRequest`], not a silent conversion — and the length is checked with
    /// `==`, not `>=`: an oversized slice is as much a caller error as an undersized one.
    ///
    /// On a part-way failure the buffer is left in an **unspecified** state, holding any
    /// mixture of decoded and stale data. It is the caller's buffer, so the library neither
    /// zeroes it nor restores it; a caller reusing one across frames must treat a failed
    /// decode as having invalidated it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a destination whose variant or length does not match, and
    /// for a position reporting no sample format or no geometry. The class a **declined**
    /// position reports, which is where [`Header::decline_reason`] becomes an error.
    /// [`Error::LimitExceeded`] for the total-samples and decoded-output-byte caps. Then
    /// whatever [`Chunks::next_chunk`] raises for the rest of the decode.
    pub fn read_samples_into(&mut self, dst: &mut Samples) -> Result<()> {
        let header = self.current_header_for_pixels()?;
        let format = header.sample_format().ok_or_else(|| {
            Error::invalid_request("read_samples_into: this position reports no sample format")
        })?;
        if dst.format() != format {
            return Err(Error::invalid_request(format!(
                "read_samples_into: destination is {:?}, the file stores {format:?}",
                dst.format()
            )));
        }
        let expected = self.destination_len_of(&header)?;
        if dst.len() != expected {
            return Err(Error::invalid_request(format!(
                "read_samples_into: destination holds {} samples, this image produces {expected}",
                dst.len()
            )));
        }
        self.check_output_bytes(output_bytes(expected, u64::from(format.bytes()))?)?;

        let mut chunks = self.chunks();
        while let Some(chunk) = chunks.next_chunk()? {
            copy_samples(&chunk, dst)?;
        }
        Ok(())
    }

    /// The allocating convenience wrapper over [`Reader::read_samples_into`], mirroring what
    /// [`Reader::read_image`] is to [`Reader::read_image_into`].
    ///
    /// The buffer is sized from [`Reader::destination_len`] and typed from the header's own
    /// [`sample_format`](Header::sample_format), so the two ways a hand-built destination is
    /// rejected — a wrong length, a wrong variant — cannot arise. A position reporting no
    /// sample format is [`Error::InvalidRequest`], there being no variant to allocate.
    ///
    /// On failure this yields an error and **no** `Samples`, never a half-filled buffer.
    ///
    /// # Errors
    ///
    /// As [`Reader::read_samples_into`], less the two the sizing removes: a wrong length and a
    /// wrong variant cannot arise. A position reporting no sample format is
    /// [`Error::InvalidRequest`], there being no variant to allocate.
    pub fn read_samples(&mut self) -> Result<Samples> {
        let header = self.current_header_for_pixels()?;
        let format = header.sample_format().ok_or_else(|| {
            Error::invalid_request("read_samples: this position reports no sample format")
        })?;
        let expected = self.destination_len_of(&header)?;
        self.check_output_bytes(output_bytes(expected, u64::from(format.bytes()))?)?;
        let mut dst = Samples::zeroed(format, expected);
        self.read_samples_into(&mut dst)?;
        Ok(dst)
    }

    /// Decode normalized `f32` into a caller-owned buffer.
    ///
    /// The destination length is `width * height * channels` as the *header* reports them,
    /// which is `width * height` after `select_channel`. Checked with `==`.
    ///
    /// Refused when no representable range is in force — [`Error::Unsupported`] for a source
    /// whose format defines no default (a FITS float frame, or FITS integer scaling outside
    /// the unsigned convention), [`Error::Malformed`] for an image whose declared `bounds` is
    /// missing or invalid. [`Reader::set_bounds`] is the escape hatch for both. Native
    /// samples still decode in either case: a frame that cannot be *normalized* is not
    /// thereby a frame that cannot be *read*.
    ///
    /// On a part-way failure the buffer is left in an unspecified state, as
    /// [`Reader::read_samples_into`] describes.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a destination of the wrong length and for a position
    /// reporting no geometry. The class a **declined** position reports.
    /// [`Error::LimitExceeded`] for the total-samples and decoded-output-byte caps.
    /// [`Error::Unsupported`] or [`Error::Malformed`] where no representable range is in
    /// force, as above. Then whatever [`Chunks::next_chunk`] raises for the rest of the
    /// decode.
    pub fn read_image_into(&mut self, dst: &mut [f32]) -> Result<()> {
        let header = self.current_header_for_pixels()?;
        let expected = self.destination_len_of(&header)?;
        if dst.len() != expected {
            return Err(Error::invalid_request(format!(
                "read_image_into: destination holds {} samples, this image produces {expected}",
                dst.len()
            )));
        }
        self.check_output_bytes(output_bytes(expected, 4)?)?;

        let normalizer = self.normalizer_for(&header)?;

        let mut chunks = self.chunks();
        while let Some(chunk) = chunks.next_chunk()? {
            let range = chunk.range();
            // Not a file defect: `range` comes from this same call's own chunk arithmetic,
            // already checked above against `dst.len() == expected`. A mismatch here can only
            // be this crate's chunking disagreeing with itself, and `Error::Malformed` would
            // misreport that as a bad file — exactly the report a caller's documented "skip
            // this frame" response then quietly absorbs, hiding the regression rather than
            // surfacing it. Panicking is what the shared `slice_samples` helper in
            // `src/samples.rs` — the one both decoders' `scratch` calls — already does for the
            // identical class of failure (a decoder-internal invariant, not something any
            // file's bytes can reach).
            let out = dst.get_mut(range.clone()).unwrap_or_else(|| {
                panic!(
                    "internal invariant violated: chunk covers destination range {range:?}, \
                     beyond the {expected} sample destination this call already validated"
                )
            });
            chunk.normalize_into(&normalizer, out);
        }
        Ok(())
    }

    /// The allocating convenience wrapper over [`Reader::read_image_into`].
    ///
    /// On failure this yields an error and **no** `Image`, never a half-filled one.
    ///
    /// # Errors
    ///
    /// As [`Reader::read_image_into`], less the wrong-length destination the sizing removes.
    pub fn read_image(&mut self) -> Result<Image> {
        let header = self.current_header_for_pixels()?;
        let expected = self.destination_len_of(&header)?;
        self.check_output_bytes(output_bytes(expected, 4)?)?;
        // Asked before the destination is allocated, and **after** `check_output_bytes` so that
        // no error class reorders: a frame with no representable range — a FITS float frame is
        // the common one — is refused by `read_image_into` below whatever this does, so
        // allocating first buys a caller who declines every float frame a `width * height`
        // buffer per frame it declines. `read_image_into` builds its own; a `Normalizer` is a
        // range and a reciprocal.
        self.normalizer_for(&header)?;
        let mut data = vec![0.0f32; expected];
        self.read_image_into(&mut data)?;
        let header = self.header().expect("header exists in the pixel phase");
        Ok(Image { header, data })
    }

    /// How many samples a destination for the current image must hold.
    ///
    /// `width * height * channels` as the **header** reports them, which is `width * height`
    /// after [`Reader::select_channel`] — so it answers the question every tier-2 destination
    /// is checked against with `==`, and it answers it from the reader rather than from a
    /// header the caller may have fetched before configuring it. That staleness is the whole
    /// reason this exists: a length computed from a pre-`select_channel` header is rejected,
    /// correctly, and the caller then has to know the rule.
    ///
    /// # Errors
    ///
    /// It carries the pixel phase's first checks with it, because a length is only meaningful
    /// for a position that will decode: a **declined** position raises its own class, the
    /// total-samples cap applies, and a position reporting no geometry is
    /// [`Error::InvalidRequest`], there being no buffer size to give. Before the first
    /// [`Reader::next_image`] it is the [`Error::InvalidRequest`] [`Reader::current_header`]
    /// gives.
    pub fn destination_len(&self) -> Result<usize> {
        self.destination_len_of(&self.current_header_for_pixels()?)
    }

    /// The normalization primitive for the current image, or the error that says why the
    /// image has none.
    ///
    /// This is the tier-3 half of § The API's claim that a chunk consumer normalizes "with
    /// the same public primitive" tier 2 uses. Hand it to [`Chunk::normalize_into`] per chunk
    /// and the assembled buffer is bit-identical to [`Reader::read_image_into`] by
    /// construction — the alternative being a caller that matches a `#[non_exhaustive]`
    /// [`Bounds`] it cannot match exhaustively, reproduces this mapping of the two
    /// [`Bounds::Unavailable`] reasons onto their error classes, and writes the nine-arm
    /// sample dispatch itself, where any slip silently moves bits this crate declares to be
    /// public API.
    ///
    /// It takes **no header**: it reads the reader's current one, which is what folds
    /// [`Reader::set_bounds`] in. A caller-supplied header could be a stale one, and a stale
    /// one normalizes against the wrong range — precisely the bit-moving slip this exists to
    /// prevent. The pixel-phase rule applies for the same reason it applies to
    /// [`Header::channels`]: call [`Reader::select_channel`] and [`Reader::set_bounds`]
    /// **before** asking for this, since the primitive describes what the reader will
    /// produce.
    ///
    /// # Errors
    ///
    /// Refused where [`Reader::read_image_into`] is refused and with the same classes:
    /// [`Error::Unsupported`] for a source whose format defines no default range,
    /// [`Error::Malformed`] for an image whose declared `bounds` is missing or invalid, and
    /// [`Reader::set_bounds`] is the escape hatch for both. It carries the pixel phase's first
    /// checks too, exactly as [`Reader::destination_len`] does — a declined position raises its
    /// own class, and the total-samples cap applies.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn f(reader: &mut astroframe::Reader<impl astroframe::Source>) -> astroframe::Result<()> {
    /// let mut buffer = vec![0.0f32; reader.destination_len()?];
    /// let normalizer = reader.normalizer()?;
    /// let mut chunks = reader.chunks();
    /// while let Some(chunk) = chunks.next_chunk()? {
    ///     let range = chunk.range();
    ///     chunk.normalize_into(&normalizer, &mut buffer[range]);
    /// }
    /// # Ok(()) }
    /// ```
    pub fn normalizer(&self) -> Result<Normalizer> {
        self.normalizer_for(&self.current_header_for_pixels()?)
    }

    // ----------------------------------------------------------------- internals

    /// The header every pixel-phase entry point starts from, with the phase's first two
    /// checks already applied.
    ///
    /// The total-samples cap is one of them, and it lives here rather than at each call site
    /// because that is what fixes its place in the pixel-phase order: **first**, before the
    /// declared-size-versus-geometry cross-check `begin_pixels` runs and before either byte
    /// cap. A frame declaring an impossible sample count is `LimitExceeded` on the geometry it
    /// declared, not `Malformed` on the size cross-check and not `LimitExceeded` naming a
    /// destination that only exists because the declaration was believed.
    fn current_header_for_pixels(&self) -> Result<Header> {
        let header = self.current_header()?;
        if let Some(decline) = header.decline_reason() {
            return Err(decline.to_error());
        }
        self.check_total_samples()?;
        Ok(header)
    }

    /// The destination length in samples, from the header's own (possibly narrowed) geometry.
    fn destination_len_of(&self, header: &Header) -> Result<usize> {
        let g = header.geometry.ok_or_else(|| {
            Error::invalid_request("this position reports no geometry, so no buffer size follows")
        })?;
        narrow(g.total_samples(), "destination length")
    }

    fn check_output_bytes(&self, bytes: u64) -> Result<()> {
        if bytes > self.limits.decoded_output_bytes {
            return Err(Error::limit(format!(
                "decoded output bytes: this decode writes {bytes} bytes, above the {} byte cap",
                self.limits.decoded_output_bytes
            )));
        }
        Ok(())
    }

    /// Build the normalization primitive from a header this reader produced, or say why there
    /// is none.
    ///
    /// No re-validation: [`Bounds`] carries the [`SampleRange`] its producer already checked, so the
    /// three usable variants hand one over rather than a pair to rebuild. There is no
    /// unreachable failure branch left to invent a class for.
    fn normalizer_for(&self, header: &Header) -> Result<Normalizer> {
        let range = match header.bounds() {
            Bounds::FormatDefault(range) | Bounds::Declared(range) => *range,
            Bounds::CallerSupplied { effective, .. } => *effective,
            Bounds::Unavailable(BoundsUnavailable::NoFormatDefault) => {
                return Err(Error::unsupported(
                    "this source defines no representable range for normalized output; decode \
                     native samples, or supply one with set_bounds",
                ));
            }
            Bounds::Unavailable(BoundsUnavailable::InvalidDeclared) => {
                return Err(Error::malformed(
                    "the declared bounds are missing or fail the range validity rule; native \
                     samples still decode, and set_bounds overrides",
                ));
            }
        };
        Ok(Normalizer::new(header.scaling(), range))
    }

    /// Drive the format decoder one chunk forward, beginning the pixel phase if needed.
    /// One chunk, and the guarantee that a failed decode stays failed.
    ///
    /// **A pixel-phase error poisons the image.** A decoder that has failed part-way through a
    /// compressed block holds state that no longer describes the file — a codec abandoned
    /// mid-stream, a stream replaced by the uncompressed reader while its subblock could not be
    /// opened — and asking it for the next chunk then produces *something*, which is the one
    /// outcome § The organizing principle rules out: "a frame it cannot decode under its
    /// documented rules is an error, never a best guess". Left ungated, a tier-3 caller that
    /// logs an error and keeps pulling was handed the block's own compressed bytes as pixels
    /// and then a clean end of image.
    ///
    /// It is `InvalidRequest` rather than a repeat of the original error: the file's fault was
    /// already reported once, and pulling again after being told the decode failed is the
    /// caller's. `next_image` clears it, so the poison scopes to the image that failed.
    fn advance_chunk(&mut self) -> Result<Option<ChunkMeta>> {
        if self.pixels_failed {
            return Err(Error::invalid_request(
                "this image's decode already failed; its remaining chunks cannot be trusted. \
                 Advance to the next image, or start a fresh cursor with chunks() to retry \
                 this one — which a seekable source allows and a sequential one refuses",
            ));
        }
        let out = self.advance_chunk_inner();
        if out.is_err() {
            self.pixels_failed = true;
        }
        out
    }

    fn advance_chunk_inner(&mut self) -> Result<Option<ChunkMeta>> {
        if !self.pixels_started {
            // Carries the total-samples cap with it, which is why it runs before anything
            // sizes a buffer.
            self.current_header_for_pixels()?;
            let plan = PixelPlan {
                selected_channel: self.selected_channel,
            };
            let src = &mut self.source;
            let limits = &self.limits;
            dispatch!(&mut self.inner, d => d.begin_pixels(src, limits, &plan), Result<()>)?;
            self.pixels_started = true;
        }
        let src = &mut self.source;
        let limits = &self.limits;
        dispatch!(&mut self.inner, d => d.next_chunk(src, limits), Result<Option<ChunkMeta>>)
    }

    /// The total-samples cap, evaluated **first** in the pixel phase, on the **file's**
    /// declared geometry alone — which is why every tier-2 entry point calls it before it
    /// touches its byte cap, and `advance_chunk` calls it before the decoder is started. It is
    /// idempotent, so running it at both places costs nothing.
    ///
    /// `select_channel` does not narrow what it counts: it is the check that runs before any
    /// buffer is sized and before any sample width is known, and rejecting a hostile
    /// declaration that early is the whole of what it is for. Fixing it first is what makes a
    /// frame declaring an impossible sample count `LimitExceeded` rather than `Malformed` on
    /// the size cross-check.
    fn check_total_samples(&self) -> Result<()> {
        let header = dispatch!(&self.inner, d => d.header(), Option<&Header>);
        let Some(g) = header.and_then(|h| h.geometry) else {
            return Ok(());
        };
        let total = g.total_samples();
        if total > self.limits.total_samples {
            return Err(Error::limit(format!(
                "total samples: the file declares {}x{}x{} = {total} samples, above the {} cap",
                g.width, g.height, g.channels, self.limits.total_samples
            )));
        }

        // The declared sample count must also have a *byte* extent this crate can compute, and
        // that is a separate question from the cap above, because the cap is the caller's to
        // raise — § The caps contemplates a workstation tool raising it, and this repo's own
        // tests set it to `u64::MAX`. At that setting every geometry passes the comparison, and
        // the products downstream of it (a staging row, the destination's byte size, a sample's
        // offset within the block) are then free to overflow `u64`: panicking under the
        // overflow checks every test and fuzz build runs with, and wrapping into an undersized
        // buffer or an absurd allocation elsewhere.
        //
        // This bounds the *products of the geometry* — a staging row, the destination's byte
        // size, a sample's offset within the block — because each is at most `total x width`,
        // and `width` is the widest of the stored sample and the `f32` this crate normalizes
        // into, both being sized from the same count. It does **not** bound everything
        // downstream, and claiming so would be the kind of comment this crate has already been
        // caught by: two sites add a *file offset* on top of such a product (a FITS data-unit
        // start, an XISF attachment position), and an offset is not a factor of the geometry.
        // So this is one cheap check that removes a whole class early, not a proof that covers
        // those sites.
        //
        // The two are bounded differently, and the tempting claim that both are "written
        // checked in their own right" holds for only one of them:
        //
        // * FITS's `data_start` is a position the source **reached**, so it is bounded by the
        //   reading. And `file_row * row_bytes` is strictly less than the frame's byte extent,
        //   which this gate has already refused if it does not fit `u64`. Nothing is left over
        //   for the sum to overflow into.
        //   (`fits_caps::a_row_offset_past_u64_is_refused_rather_than_computed` asserts it.)
        // * XISF's `position` is **declared**, parsed straight out of a `location` attribute,
        //   and no reading bounds it on a source whose length the reader cannot see. Written as
        //   a bare `+` that site overflowed, so it is `checked_mul`/`checked_add`.
        //   (`adversarial::a_row_offset_past_u64_is_refused_rather_than_computed`.)
        //
        // The rule the two divide on: a **declared** offset needs its own arithmetic checked,
        // an **observed** one is bounded by the bytes that produced it.
        //
        // Overflow is `LimitExceeded` rather than `Malformed`: the file may be perfectly
        // well-formed, and it is this crate's address space that cannot hold it — the same
        // answer the narrowing rule gives.
        let width = header
            .and_then(|h| h.sample_format)
            .map_or(4, |f| u64::from(f.bytes()).max(4));
        if total.checked_mul(width).is_none() {
            return Err(Error::limit(format!(
                "total samples: the file declares {}x{}x{} = {total} samples, whose size in \
                 bytes overflows the 64-bit arithmetic every size computation here runs in",
                g.width, g.height, g.channels
            )));
        }
        Ok(())
    }

    fn scratch_slice(&self, len: usize) -> SampleSlice<'_> {
        dispatch!(&self.inner, d => d.scratch(len), SampleSlice<'_>)
    }
}

/// A destination's size in bytes, checked.
///
/// The gate in `check_total_samples` already rules out a sample count whose byte extent
/// overflows, so this cannot fail today. It is written checked anyway because § The caps makes
/// that the rule for every size computation, and because the gate's guarantee is an argument
/// rather than a local fact: this is the site that would silently wrap if the argument ever
/// stopped holding, and wrapping here sizes an allocation from the remainder.
fn output_bytes(samples: usize, width: u64) -> Result<u64> {
    (samples as u64).checked_mul(width).ok_or_else(|| {
        Error::limit(format!(
            "decoded output bytes: {samples} samples at {width} bytes each overflows the \
             64-bit arithmetic every size computation here runs in"
        ))
    })
}

/// Copy one chunk into its slice of a native-sample destination.
fn copy_samples(chunk: &Chunk<'_>, dst: &mut Samples) -> Result<()> {
    macro_rules! arm {
        ($src:expr, $variant:ident) => {{
            // `read_samples_into` has already required the destination's variant to match
            // the header's sample format, the chunk's variant comes from scratch built from
            // that same format, and `dst` is borrowed mutably for the whole loop, so it
            // cannot change underneath. A mismatch is this crate contradicting itself, not a
            // caller mistake -- and `InvalidRequest` would have told the caller to fix
            // something they did nothing wrong in.
            let Samples::$variant(out) = dst else {
                unreachable!(
                    "internal invariant violated: the destination's sample variant changed \
                     mid-decode, after `read_samples_into` matched it against the header"
                );
            };
            let range = chunk.range();
            // See the matching comment in `read_image_into`: this is the same decoder-internal
            // invariant, not a file defect, so it panics rather than misreporting `Malformed`.
            let target = out.get_mut(range.clone()).unwrap_or_else(|| {
                panic!("internal invariant violated: chunk covers destination range {range:?}, beyond it")
            });
            target.copy_from_slice($src);
            Ok(())
        }};
    }
    match chunk.samples() {
        SampleSlice::U8(s) => arm!(s, U8),
        SampleSlice::U16(s) => arm!(s, U16),
        SampleSlice::U32(s) => arm!(s, U32),
        SampleSlice::U64(s) => arm!(s, U64),
        SampleSlice::I16(s) => arm!(s, I16),
        SampleSlice::I32(s) => arm!(s, I32),
        SampleSlice::I64(s) => arm!(s, I64),
        SampleSlice::F32(s) => arm!(s, F32),
        SampleSlice::F64(s) => arm!(s, F64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The auto-trait claim in `Reader`'s own doc comment and in § The API, pinned so it
    /// cannot rot again. It already had: both homes said `Reader` is "deliberately **not**
    /// `Sync`" on the grounds that every useful method takes `&mut self` — an argument about
    /// the API's usefulness, not about the soundness of sharing a `&T`, and the compiler
    /// never agreed with it. A property asserted only in prose is a property nothing checks.
    #[test]
    fn reader_is_send_and_sync_when_its_source_is() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Reader<crate::source::Seekable<std::io::Cursor<Vec<u8>>>>>();
        assert_sync::<Reader<crate::source::Seekable<std::io::Cursor<Vec<u8>>>>>();
    }

    /// A chunk's destination range disagreeing with the buffer it was already
    /// checked against is this crate's own chunking arithmetic contradicting itself, not
    /// anything a file can cause — so it panics instead of returning `Error::Malformed`,
    /// matching `slice_samples` in `src/samples.rs`, which both decoders reach through
    /// `scratch` and which faces the identical class of failure.
    #[test]
    #[should_panic(expected = "internal invariant violated")]
    fn copy_samples_panics_when_a_chunks_range_exceeds_the_destination() {
        let chunk = Chunk {
            channel: 0,
            range: 0..4,
            samples: SampleSlice::U8(&[1, 2, 3, 4]),
        };
        let mut dst = Samples::U8(vec![0u8; 2]);
        let _ = copy_samples(&chunk, &mut dst);
    }

    /// At `images_per_source = u32::MAX`, a `u32` advance counter's
    /// `saturating_add(1)` sticks at `u32::MAX` instead of crossing it, so the cap comparison
    /// never fires. Reaches into `Reader`'s private fields directly rather than actually
    /// driving four billion advances.
    #[cfg(feature = "xisf")]
    #[test]
    fn images_per_source_cap_still_trips_at_u32_max() {
        let two_images = concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<xisf xmlns="http://www.pixinsight.com/xisf" version="1.0">"#,
            r#"<Image geometry="1:1:1" sampleFormat="UInt8" location="embedded">"#,
            r#"<Data encoding="hex">00</Data></Image>"#,
            r#"<Image geometry="1:1:1" sampleFormat="UInt8" location="embedded">"#,
            r#"<Data encoding="hex">00</Data></Image></xisf>"#
        );
        let mut bytes = b"XISF0100".to_vec();
        bytes.extend_from_slice(&(two_images.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(two_images.as_bytes());

        let limits = Limits {
            images_per_source: u32::MAX,
            ..Limits::default()
        };
        let mut reader =
            Reader::sequential_with_limits(std::io::Cursor::new(bytes), limits).unwrap();
        assert!(reader.next_image().unwrap(), "the fixture's first image");

        reader.advances = u64::from(u32::MAX);

        let err = reader
            .next_image()
            .expect_err("the fixture's second image crosses the cap from u32::MAX advances");
        assert!(
            matches!(err, Error::LimitExceeded(_)),
            "unexpected error: {err:?}"
        );
    }

    /// `set_bounds` overriding a file-declared `bounds` must report the file's own
    /// text verbatim, not a numeric pair re-rendered through a formatter — `1.500e+03` becomes
    /// `1500` that way, which is precisely the "re-rendering a number through a formatter can
    /// lose digits" failure § Decisions the implementer must not silently change bans for keyword values.
    #[cfg(feature = "xisf")]
    #[test]
    fn overriding_a_declared_bounds_preserves_the_files_verbatim_text() {
        // A minimal monolithic XISF unit, built by hand rather than through
        // `tests/common/xisf.rs`: this module cannot reach that helper (it lives outside a
        // Reader unit test's edit boundary here), and one embedded-block image needs none of
        // its attachment-offset machinery.
        let image = concat!(
            r#"<Image geometry="1:1:1" sampleFormat="UInt8" bounds="0.0000:1.500e+03" "#,
            r#"location="embedded"><Data encoding="hex">00</Data></Image>"#
        );
        let header = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><xisf xmlns="http://www.pixinsight.com/xisf" version="1.0">{image}</xisf>"#
        );
        let mut bytes = b"XISF0100".to_vec();
        bytes.extend_from_slice(&(header.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(header.as_bytes());

        let mut reader = Reader::sequential(std::io::Cursor::new(bytes)).unwrap();
        assert!(reader.next_image().unwrap());
        assert!(
            matches!(reader.header().unwrap().bounds(), Bounds::Declared(r) if r.lo() == 0.0 && r.hi() == 1500.0),
            "fixture sanity check: the file's declared bounds must parse before the override \
             matters"
        );
        reader.set_bounds(0.0, 100.0).unwrap();

        match reader.header().unwrap().bounds() {
            Bounds::CallerSupplied {
                declared,
                effective,
            } => {
                assert_eq!(declared.as_deref(), Some("0.0000:1.500e+03"));
                assert_eq!(effective.lo(), 0.0);
                assert_eq!(effective.hi(), 100.0);
            }
            other => panic!("unexpected bounds: {other:?}"),
        }
    }
}
