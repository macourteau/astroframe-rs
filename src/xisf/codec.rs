//! Decompression and checksum verification.
//!
//! The three codecs have three different container shapes, and getting one wrong fails every
//! block of that codec. **Dispatch on the declared `compression` name, never on the bytes** —
//! only `zstd` has a real magic number, so sniffing is not available even in principle:
//!
//! | Declared codec | Container shape |
//! | --- | --- |
//! | `lz4`, `lz4hc` | A **bare block**, no frame header. The frame magic `04 22 4d 18` is absent; leading bytes are payload |
//! | `zlib` | **zlib-wrapped**, not raw deflate — `78 9c` and friends |
//! | `zstd` | **Framed**, `28 b5 2f fd` |
//!
//! LZ4 and zstd fail in *opposite* directions: reaching for a framed LZ4 reader breaks LZ4,
//! and reaching for a bare-block zstd reader breaks zstd.

use std::io::Read;

use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::source::Source;
use crate::xisf::block::{Codec, Compression, Subblock};

#[cfg(feature = "checksum")]
use crate::xisf::block::{Algorithm, Checksum};

/// Verify a stored block against its declared digest.
///
/// The digest covers the *entire* contents of the block as stored — for a compressed block
/// that is the compressed bytes, never the original (§10.6.1). §10.5 tells a decoder to warn
/// and let *the user* choose whether to continue; for a library the caller is that user, and
/// the same section adds that an unattended batch decoder should not load a failing unit,
/// which is this crate's primary setting.
#[cfg(feature = "checksum")]
pub(crate) fn verify(checksum: &Checksum, stored: &[u8]) -> Result<()> {
    use sha1::Digest as _;

    let computed: Vec<u8> = match checksum.algorithm {
        Algorithm::Sha1 => sha1::Sha1::digest(stored).to_vec(),
        Algorithm::Sha256 => sha2::Sha256::digest(stored).to_vec(),
        Algorithm::Sha512 => sha2::Sha512::digest(stored).to_vec(),
        Algorithm::Sha3_256 => sha3::Sha3_256::digest(stored).to_vec(),
        Algorithm::Sha3_512 => sha3::Sha3_512::digest(stored).to_vec(),
    };
    if computed == checksum.digest {
        return Ok(());
    }
    Err(Error::checksum(format!(
        "block digest is {}, the header declares {}",
        hex(&computed),
        hex(&checksum.digest)
    )))
}

/// Without the `checksum` feature the verification code and its three hash dependencies are
/// not compiled in, so a block that declares a checksum is refused rather than trusted.
///
/// That does not weaken the guarantee — it narrows what such a build decodes. §9.5 and §10.5
/// require every block of a **digitally signed** unit that is not serialized directly in the
/// header to carry a checksum, so this build cannot decode a signed unit whose pixels are
/// attached.
#[cfg(not(feature = "checksum"))]
pub(crate) fn verify(_checksum: &crate::xisf::block::Checksum, _stored: &[u8]) -> Result<()> {
    Err(Error::unsupported(
        "this block declares a checksum and this build has the `checksum` feature disabled; \
         §10.5 makes verification mandatory when the attribute is present",
    ))
}

#[cfg(feature = "checksum")]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Decompress a stored block into a destination sized from the **geometry**, never from a
/// declared length.
///
/// Every codec here inflates to exactly `out.len()` and is then required to be at end of
/// stream, so a decompression bomb is refused after a few dozen bytes rather than after
/// materializing. The declared uncompressed size has already been cross-checked against the
/// geometry before this is called, which is what makes bombs structurally impossible rather
/// than merely caught.
pub(crate) fn decompress(
    stored: &[u8],
    compression: &Compression,
    subblocks: Option<&[Subblock]>,
    out: &mut [u8],
    limits: &Limits,
) -> Result<()> {
    // One codec state for the whole block, whatever the split. `Stream::open` carries one
    // across the streaming path for the same reason [`Codecs`] gives.
    let mut codecs = Codecs::default();

    let Some(list) = subblocks else {
        return decompress_one(&mut codecs, stored, compression.codec, out, limits);
    };

    // §10.6's subblock split is over the *compression*, so each pair decompresses
    // independently into its own run of the destination.
    let mut in_at = 0usize;
    let mut out_at = 0usize;
    for (index, (compressed, uncompressed)) in list.iter().enumerate() {
        let compressed = usize::try_from(*compressed)
            .map_err(|_| Error::malformed("subblocks: a compressed length does not fit usize"))?;
        let uncompressed = usize::try_from(*uncompressed).map_err(|_| {
            Error::malformed("subblocks: an uncompressed length does not fit usize")
        })?;
        let src = stored
            .get(in_at..in_at + compressed)
            .ok_or_else(|| Error::malformed(format!("subblock {index} runs past the block")))?;
        let dst = out.get_mut(out_at..out_at + uncompressed).ok_or_else(|| {
            Error::malformed(format!(
                "subblock {index} runs past the geometry-implied size"
            ))
        })?;
        decompress_one(&mut codecs, src, compression.codec, dst, limits)?;
        in_at += compressed;
        out_at += uncompressed;
    }
    Ok(())
}

/// The framed codecs' state, carried across a block's subblocks.
///
/// §10.6 restarts the codec at every subblock boundary, so a state built at a boundary is
/// built `Subblock count` times — 4096 — from a block whose stored bytes are whatever the file
/// felt like. `docs/intentional-patterns.md` states the rule that breaks: **no per-occurrence
/// allocation may be sized by a cap**. A `flate2::read::ZlibDecoder` costs about 76 KB to
/// construct (a `Decompress` and the 32 KiB buffer its reader wraps) and a
/// `ruzstd::FrameDecoder` about 9 KB, and neither figure has anything to do with the subblock
/// it decodes.
///
/// Both codecs' resets keep the buffers and clear the state, which is the same decode from a
/// clean stream position. The peak is unchanged: at most one state exists, because a block
/// declares one codec, and reuse holds strictly less than construction did.
///
/// LZ4 is absent because it has no state to carry — `lz4_flex::block::decompress_into`
/// allocates no codec and writes straight into the destination.
#[derive(Default)]
struct Codecs {
    zlib: Option<flate2::Decompress>,
    zstd: Option<Box<ruzstd::decoding::FrameDecoder>>,
}

fn decompress_one(
    codecs: &mut Codecs,
    stored: &[u8],
    codec: Codec,
    out: &mut [u8],
    limits: &Limits,
) -> Result<()> {
    match codec {
        Codec::Lz4 | Codec::Lz4Hc => decompress_lz4(stored, out),
        // zlib-wrapped, not raw deflate: feeding these bytes to an inflate-raw reader
        // type-checks and then fails on every block.
        Codec::Zlib => {
            let d = codecs
                .zlib
                .get_or_insert_with(|| flate2::Decompress::new(true));
            d.reset(true);
            exact_stream(
                Inflate {
                    d,
                    input: stored,
                    done: false,
                },
                out,
                "zlib",
            )
        }
        // Framed, so a streaming decoder applies. The frame header declares a window size and
        // a zstd decoder allocates that window before producing a byte -- the precise shape of
        // "an allocation sized from an unvalidated declared size" -- so the cap is set on the
        // decoder before the frame is read, and the read checks it before the allocation.
        Codec::Zstd => {
            let fd = codecs
                .zstd
                .get_or_insert_with(|| Box::new(ruzstd::decoding::FrameDecoder::new()));
            fd.set_max_window_size(limits.zstd_window_bytes);
            let decoder = ruzstd::decoding::StreamingDecoder::new_with_decoder(stored, fd.as_mut())
                .map_err(|e| zstd_header_error(e, limits))?;
            exact_stream(decoder, out, "zstd")
        }
    }
}

/// A bare LZ4 block: no frame header, decompresses only as a whole, and the output length is
/// known from the geometry rather than read from the stream.
fn decompress_lz4(stored: &[u8], out: &mut [u8]) -> Result<()> {
    let written = lz4_flex::block::decompress_into(stored, out)
        .map_err(|e| Error::malformed(format!("LZ4 block does not decompress: {e}")))?;
    if written != out.len() {
        return Err(Error::malformed(format!(
            "LZ4 block inflates to {written} bytes where the geometry implies {}",
            out.len()
        )));
    }
    Ok(())
}

/// A `Read` over one subblock's stored bytes, driving a decompressor the whole block shares.
///
/// `flate2::read::ZlibDecoder` is the obvious spelling and is the one thing that cannot be
/// reused: its own `reset` **builds a fresh `Decompress`** rather than resetting the one it
/// holds, so a decoder carried across the subblock list that way costs exactly what
/// constructing one per subblock costs. `Decompress::reset` is the reset that keeps the
/// buffers, and it is reached only by driving the decompressor directly.
///
/// Every subblock is resident before this runs, so the whole of it is offered on each call and
/// `Finish` is the honest flush: there is no more input coming.
struct Inflate<'a> {
    d: &'a mut flate2::Decompress,
    input: &'a [u8],
    /// Set once the stream has ended, so that the byte `exact_stream` reads past the
    /// destination reaches a spent decompressor rather than an undefined one.
    done: bool,
}

impl Read for Inflate<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        let before_in = self.d.total_in();
        let before_out = self.d.total_out();
        let status = self
            .d
            .decompress(self.input, out, flate2::FlushDecompress::Finish)
            .map_err(std::io::Error::other)?;
        let consumed = (self.d.total_in() - before_in) as usize;
        let produced = (self.d.total_out() - before_out) as usize;
        self.input = &self.input[consumed..];
        // A finished stream and an iteration that moved neither counter both mean there is
        // nothing further to give, which a reader says by returning zero: `exact_stream` turns
        // a short read into the truncation it is, and its one-byte read past the destination
        // catches a block that inflates too far. Trailing bytes after the stream's end are
        // ignored rather than refused, which is the treatment a reader over the same
        // decompressor gives them.
        self.done = matches!(status, flate2::Status::StreamEnd) || (produced == 0 && consumed == 0);
        Ok(produced)
    }
}

/// Fill `out` exactly, then require end of stream.
fn exact_stream(mut reader: impl Read, out: &mut [u8], codec: &str) -> Result<()> {
    reader
        .read_exact(out)
        .map_err(|e| Error::malformed(format!("{codec} block does not decompress: {e}")))?;
    let mut extra = [0u8; 1];
    match reader.read(&mut extra) {
        Ok(0) => Ok(()),
        Ok(_) => Err(Error::malformed(format!(
            "{codec} block inflates past the {} bytes the geometry implies",
            out.len()
        ))),
        Err(e) => Err(Error::malformed(format!(
            "{codec} block does not decompress: {e}"
        ))),
    }
}

/// Decode an `embedded` or `inline` block's text.
///
/// §10.3 says white space "is irrelevant and *must* be ignored" for both encodings and the
/// specification's own embedded example is line-wrapped — but §11.1.6 says the opposite for a
/// `String` property's character data, whose white space "a compliant decoder must preserve".
/// Both surfaces are read by the same parser, so stripping happens **here**, at the decode
/// site, and never at the reader.
pub(crate) fn decode_text(text: &str, encoding: crate::xisf::block::Encoding) -> Result<Vec<u8>> {
    use crate::xisf::block::Encoding;
    match encoding {
        Encoding::Hex => crate::xisf::block::decode_hex(text)
            .map_err(|()| Error::malformed("embedded block is not lowercase Base16")),
        Encoding::Base64 => {
            use base64::Engine as _;
            let stripped: Vec<u8> = text
                .bytes()
                .filter(|b| !crate::xisf::scalars::is_xml_space(*b))
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(stripped)
                .map_err(|e| Error::malformed(format!("embedded block is not Base64: {e}")))
        }
    }
}

/// Reads the stored block **straight from the source**, bounded by what is left of it.
///
/// This is the point of the streaming shape. Queuing compressed bytes in a buffer the caller
/// tops up between reads cannot work: a codec that drains the queue *inside* one call is
/// handed a zero-length read, which `flate2` turns into `MZFlush::Finish` and `ruzstd`
/// reports as `UnexpectedEof` — both on perfectly good files. No threshold fixes it, because
/// a codec may consume any amount in one call; measured against the corpus, `ruzstd` swallowed
/// a full mebibyte and then asked for 23 KiB more.
///
/// A reader that borrows the source for the duration of the call cannot be starved. It returns
/// zero exactly where the block ends, which is the one place a codec is right to conclude the
/// stream is over.
struct BlockReader<'a, S: Source> {
    src: &'a mut S,
    limits: &'a Limits,
    /// Stored bytes of the current subblock not yet handed to the codec.
    left: &'a mut u64,
    /// Where the next of them sits in the file.
    at: &'a mut u64,
}

impl<S: Source> Read for BlockReader<'_, S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if *self.left == 0 || out.is_empty() {
            return Ok(0);
        }
        let want = (*self.left).min(out.len() as u64) as usize;
        if self.src.position() != *self.at {
            self.src
                .seek_to(*self.at, self.limits)
                .map_err(std::io::Error::other)?;
        }
        self.src
            .read_exact(&mut out[..want])
            .map_err(std::io::Error::other)?;
        *self.left -= want as u64;
        *self.at += want as u64;
        Ok(want)
    }
}

/// How much compressed input to pull in one go.
const INPUT_CHUNK: usize = 256 * 1024;

/// The input window a subblock is read through: a chunk, or the subblock itself when the
/// subblock is smaller.
///
/// **The `min` is the whole point.** §10.6 restarts the codec at every subblock boundary, so
/// this window is a per-occurrence allocation whose count is `Subblock count` — and
/// `docs/intentional-patterns.md` states that no per-occurrence allocation may be sized by a
/// cap. A flat `INPUT_CHUNK` is exactly that: 4096 subblocks of fifteen stored bytes each cost
/// a gigabyte of zeroed buffer, which § Fuzzing's oracle sees in full because it counts every
/// allocation rather than the peak. Sized from the subblock, the window is bounded by that
/// subblock's own stored bytes, and the sum over a block is bounded by the block.
fn input_window(stored_left: u64) -> usize {
    usize::try_from(stored_left)
        .unwrap_or(INPUT_CHUNK)
        .min(INPUT_CHUNK)
}

/// LZ4 state: one whole subblock, decompressed, and how much of it has been handed over.
///
/// LZ4 as XISF stores it is a bare block with no framing, so nothing smaller than a whole
/// subblock can be produced -- which is exactly the `Block` floor `granularity()` reports, and
/// exactly the peak memory § Streaming promises for it: one subblock, compressed and
/// decompressed, rather than the whole image.
pub(crate) struct Lz4State {
    buf: Vec<u8>,
    at: usize,
}

/// zlib state: the raw decompressor plus the input window it is fed from.
pub(crate) struct ZlibState {
    d: flate2::Decompress,
    buf: Vec<u8>,
    at: usize,
    end: usize,
}

/// zstd state: the frame decoder, plus what it has produced and not yet handed over.
pub(crate) struct ZstdState {
    fd: ruzstd::decoding::FrameDecoder,
    pending: Vec<u8>,
    at: usize,
}

/// How the streaming path pulls rows.
pub(crate) enum Stream {
    /// Uncompressed: rows come straight from the source at computed offsets.
    Plain,
    /// zlib-wrapped, decompressing incrementally.
    Zlib(Box<ZlibState>),
    /// Framed zstd, likewise incremental, with the declared window capped before the frame
    /// header is read.
    Zstd(Box<ZstdState>),
    /// Bare LZ4, one subblock held whole.
    Lz4(Box<Lz4State>),
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Stream::Plain => "Plain",
            Stream::Zlib(_) => "Zlib",
            Stream::Zstd(_) => "Zstd",
            Stream::Lz4(_) => "Lz4",
        })
    }
}

impl Stream {
    /// Open `codec` over the subblock the cursors describe, **reusing this stream's state**
    /// where the codec already matches.
    ///
    /// §10.6 splits the compression, so this runs once per subblock and the number of times is
    /// `Subblock count` rather than anything the file's length bounds. Both framed codecs
    /// therefore carry one state across the whole block and reset it at each boundary — a
    /// `flate2::Decompress` costs about 43 KB to construct and a `ruzstd::FrameDecoder` about
    /// 9.5 KB, and constructing either per subblock multiplies a fixed cost by a cap.
    /// `Decompress::reset` and `FrameDecoder::reset` both keep the buffers and reset the
    /// state, which is the same decode from a clean stream position.
    ///
    /// The peak is unchanged and § Streaming's promise of one subblock at a time still holds:
    /// reuse holds strictly less than construction did, and the LZ4 arm — whose buffers are
    /// the whole-subblock ones the promise is about — drops the outgoing subblock's before it
    /// sizes the incoming one's.
    pub(crate) fn open<S: Source>(
        &mut self,
        codec: Codec,
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        uncompressed: u64,
        limits: &Limits,
    ) -> Result<()> {
        match codec {
            Codec::Zlib => {
                let want = input_window(*left);
                if let Stream::Zlib(z) = self {
                    z.d.reset(true);
                    z.at = 0;
                    z.end = 0;
                    // The window only ever grows, and it grows to the subblock that asked for
                    // it, so the sum over a block is bounded by the block's stored bytes. The
                    // old buffer is released before the larger one is sized, since nothing in
                    // it survives a reset.
                    if z.buf.len() < want {
                        z.buf = Vec::new();
                        z.buf = vec![0u8; want];
                    }
                } else {
                    *self = Stream::Plain;
                    *self = Stream::Zlib(Box::new(ZlibState {
                        d: flate2::Decompress::new(true),
                        buf: vec![0u8; want],
                        at: 0,
                        end: 0,
                    }));
                }
                Ok(())
            }
            Codec::Zstd => {
                let mut z = match std::mem::replace(self, Stream::Plain) {
                    Stream::Zstd(z) => z,
                    _ => Box::new(ZstdState {
                        fd: ruzstd::decoding::FrameDecoder::new(),
                        pending: Vec::new(),
                        at: 0,
                    }),
                };
                z.fd.set_max_window_size(limits.zstd_window_bytes);
                z.pending = Vec::new();
                z.at = 0;
                let reader = BlockReader {
                    src,
                    limits,
                    left,
                    at,
                };
                // A zstd decoder allocates the window the frame header declares before
                // producing a byte -- the precise shape of an allocation sized from an
                // unvalidated declared size -- so the cap is enforced here, before the
                // allocation, and a trip is LimitExceeded like any other cap.
                z.fd.reset(reader)
                    .map_err(|e| zstd_header_error(e, limits))?;
                *self = Stream::Zstd(z);
                Ok(())
            }
            Codec::Lz4 | Codec::Lz4Hc => {
                // Drop the outgoing subblock's buffers *before* sizing the incoming one's:
                // these are the whole-subblock buffers § Streaming's peak-memory row is
                // about, and holding two of them at once is twice what it promises.
                *self = Stream::Plain;
                // Both allocations are sized from a declared length, so both are capped
                // before they are made. `materialized_bytes` is the right cap: this is the
                // one buffer a `Block`-granularity decode materializes.
                let compressed_len = check_materialized(*left, limits, "an LZ4 subblock's stored")?;
                let out_len = check_materialized(uncompressed, limits, "an LZ4 subblock's")?;
                let mut stored = vec![0u8; compressed_len];
                let mut reader = BlockReader {
                    src,
                    limits,
                    left,
                    at,
                };
                reader.read_exact(&mut stored).map_err(|e| {
                    Error::malformed(format!("LZ4 subblock's stored bytes are unreadable: {e}"))
                })?;
                let mut buf = vec![0u8; out_len];
                decompress_lz4(&stored, &mut buf)?;
                *self = Stream::Lz4(Box::new(Lz4State { buf, at: 0 }));
                Ok(())
            }
        }
    }

    /// Fill `out` exactly, pulling compressed input from the source as the codec asks for it.
    pub(crate) fn fill<S: Source>(
        &mut self,
        out: &mut [u8],
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<()> {
        let produced = self.fill_upto(out, src, left, at, limits)?;
        if produced < out.len() {
            return Err(Error::malformed(
                "compressed block is truncated: the stored bytes ran out before the \
                 geometry-implied size was reached",
            ));
        }
        Ok(())
    }

    /// Require the codec to be at end of stream.
    ///
    /// This is what refuses a decompression bomb: a block that inflates to exactly the
    /// geometry-implied size and then still has more to give is not the block the header
    /// described. It is enforced per subblock, so a bomb hidden in any one of them is caught
    /// at its own boundary.
    pub(crate) fn require_end<S: Source>(
        &mut self,
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<()> {
        if matches!(self, Stream::Plain) {
            return Ok(());
        }
        let mut probe = [0u8; 1];
        if self.fill_upto(&mut probe, src, left, at, limits)? > 0 {
            return Err(Error::malformed(
                "compressed block inflates past the size the geometry implies",
            ));
        }
        Ok(())
    }

    /// Produce up to `out.len()` bytes, returning how many. Short only at end of stream.
    fn fill_upto<S: Source>(
        &mut self,
        out: &mut [u8],
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<usize> {
        match self {
            // `next_chunk` routes an uncompressed block to its own seek-and-read arm and
            // never here, so this is the decoder contradicting itself rather than a file
            // defect — the class § Errors has no honest slot for, and the crate panics on.
            Stream::Plain => {
                unreachable!(
                    "internal invariant violated: an uncompressed block reached the streaming codec path"
                )
            }
            Stream::Zlib(z) => z.fill_upto(out, src, left, at, limits),
            Stream::Zstd(z) => z.fill_upto(out, src, left, at, limits),
            Stream::Lz4(z) => {
                let take = (z.buf.len() - z.at).min(out.len());
                out[..take].copy_from_slice(&z.buf[z.at..z.at + take]);
                z.at += take;
                Ok(take)
            }
        }
    }
}

/// Check one declared length against the materialization cap and narrow it.
pub(crate) fn check_materialized(bytes: u64, limits: &Limits, what: &str) -> Result<usize> {
    if bytes > limits.materialized_bytes {
        return Err(Error::limit(format!(
            "materialized bytes: {what} length is {bytes} bytes, above the {} byte cap",
            limits.materialized_bytes
        )));
    }
    usize::try_from(bytes).map_err(|_| {
        Error::limit(format!(
            "materialized bytes: {what} length does not fit usize"
        ))
    })
}

/// Classify a zstd frame-header failure.
///
/// Only a window above the cap is a cap trip. Everything else — a truncated header, a bad
/// magic, an unreadable dictionary id — is the file contradicting the format, and § Errors
/// makes that `Malformed`. Reporting it as `LimitExceeded` tells a consumer to retry with
/// raised limits, which can never succeed.
fn zstd_header_error(e: ruzstd::decoding::errors::FrameDecoderError, limits: &Limits) -> Error {
    use ruzstd::decoding::errors::{FrameDecoderError, FrameHeaderError};
    match &e {
        FrameDecoderError::WindowSizeTooBig { .. }
        | FrameDecoderError::FrameHeaderError(FrameHeaderError::WindowTooBig { .. })
        | FrameDecoderError::FailedToInitialize(FrameHeaderError::WindowTooBig { .. }) => {
            Error::limit(format!(
                "zstd decoder window: the frame header's declared window is above the {} byte \
                 cap ({e})",
                limits.zstd_window_bytes
            ))
        }
        _ => Error::malformed(format!("zstd frame header is not readable: {e}")),
    }
}

impl ZlibState {
    fn fill_upto<S: Source>(
        &mut self,
        out: &mut [u8],
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<usize> {
        let mut filled = 0usize;
        while filled < out.len() {
            if self.at >= self.end {
                let mut reader = BlockReader {
                    src,
                    limits,
                    left,
                    at,
                };
                let got = reader
                    .read(&mut self.buf)
                    .map_err(|e| Error::malformed(format!("reading a compressed block: {e}")))?;
                self.at = 0;
                self.end = got;
            }

            // **Input exhausted is not output exhausted.** The decompressor may hold bytes it
            // could not emit because the destination was full, so running out of stored input
            // is a reason to flush rather than to declare the block truncated. Conflating the
            // two made a subblock whose last row was already decoded report a truncation.
            let dry = self.at >= self.end;
            let flush = if dry {
                flate2::FlushDecompress::Finish
            } else {
                flate2::FlushDecompress::None
            };

            let before_in = self.d.total_in();
            let before_out = self.d.total_out();
            let status = self
                .d
                .decompress(&self.buf[self.at..self.end], &mut out[filled..], flush)
                .map_err(|e| Error::malformed(format!("zlib block does not decompress: {e}")))?;
            let consumed = (self.d.total_in() - before_in) as usize;
            let produced = (self.d.total_out() - before_out) as usize;
            self.at += consumed;
            filled += produced;

            // **Every iteration must either make progress or return.** Two ways it can fail to:
            //
            // `StreamEnd` is the one that hung. A stream that ends *before* the
            // geometry-implied size, with stored bytes still sitting unread in `buf`, leaves
            // `dry` false while the decompressor consumes nothing and produces nothing
            // forever. Eight zlib-compressed bytes and ten bytes of trailing filler are enough
            // to write it, on the commonest streamed codec, under default limits.
            //
            // The no-progress check stands beside it, unqualified by `dry`: an iteration that
            // moved neither counter has nowhere left to go whether or not input remains, since
            // the loop condition guarantees there was output space to fill. Returning short is
            // what lets `Stream::fill` raise the truncation this is.
            if matches!(status, flate2::Status::StreamEnd) || (produced == 0 && consumed == 0) {
                return Ok(filled);
            }
        }
        Ok(filled)
    }
}

impl ZstdState {
    fn fill_upto<S: Source>(
        &mut self,
        out: &mut [u8],
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<usize> {
        let mut filled = 0usize;
        while filled < out.len() {
            if self.at < self.pending.len() {
                let n = (self.pending.len() - self.at).min(out.len() - filled);
                out[filled..filled + n].copy_from_slice(&self.pending[self.at..self.at + n]);
                self.at += n;
                filled += n;
                continue;
            }
            if self.fd.is_finished() && self.fd.can_collect() == 0 {
                return Ok(filled);
            }

            // Let the decoder run even once the stored block is spent. While a frame is
            // unfinished, ruzstd **retains window_size bytes** and will not hand them over, so
            // bailing out the moment the input ran dry strands the tail of every image whose
            // last bytes sit inside that window. Reading zero is how the decoder learns the
            // frame ended, and finishing is what releases the retained bytes.
            let want = out.len() - filled;
            let reader = BlockReader {
                src,
                limits,
                left,
                at,
            };
            self.fd
                .decode_blocks(
                    reader,
                    ruzstd::decoding::BlockDecodingStrategy::UptoBytes(want),
                )
                .map_err(|e| Error::malformed(format!("zstd block does not decompress: {e}")))?;

            match self.fd.collect() {
                Some(v) if !v.is_empty() => {
                    self.pending = v;
                    self.at = 0;
                }
                _ if *left == 0 || self.fd.is_finished() => return Ok(filled),
                _ => {}
            }
        }
        Ok(filled)
    }
}
