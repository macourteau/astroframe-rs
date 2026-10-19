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
use crate::options::EqualLengthSubblocks;
use crate::source::Source;
use crate::xisf::block::{Codec, Compression, PairKind, Subblock, classify};

#[cfg(feature = "checksum")]
use crate::xisf::block::{Algorithm, Checksum};

/// Verify a stored block against its declared digest.
///
/// The digest covers the *entire* contents of the block as stored — for a compressed block
/// that is the compressed bytes, never the original (§10.6.2). §10.5 tells a decoder to warn
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
/// The refusal is `Unsupported` rather than a mismatch, and the distinction is load-bearing:
/// it is what lets the caller decline the position and keep walking, per §7's rule that an
/// unsupported feature leaves the rest of the unit accessible. Nothing unverified is ever
/// handed out either way.
///
/// What such a build gives up is conformance, not safety. §7.2 makes verification of SHA-1,
/// SHA-256 and SHA-512 an ability every decoder must have, so a build without this feature is
/// not a baseline XISF decoder. It also cannot decode a **digitally signed** unit whose pixels
/// are attached, since §9.5 and §10.5 require every block of one that is not serialized
/// directly in the header to carry a checksum.
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
///
/// Each pair is read as [`classify`] says, which is the decision `Stream::open` acts on for
/// the same pair on the streaming walk, and decoded by [`decode_pair`], which that walk also
/// calls for every pair it holds whole.
pub(crate) fn decompress(
    stored: &[u8],
    compression: &Compression,
    subblocks: Option<&[Subblock]>,
    reading: EqualLengthSubblocks,
    out: &mut [u8],
    limits: &Limits,
) -> Result<()> {
    // One codec state for the whole block, whatever the split and whatever each pair is.
    // `Stream` carries the same type across the streaming walk for the reason [`Codecs`]
    // gives.
    let mut codecs = Codecs::default();

    // §10.6.1: a block with no `subblocks` attribute is one pair, of its stored length and its
    // uncompressed size, and every rule for a pair applies to it. So the unsplit block is the
    // split one with a list of one rather than a branch of its own — which is also what makes a
    // block stored at exactly its uncompressed size an uncompressed subblock.
    let whole = [(stored.len() as u64, out.len() as u64)];
    let list = subblocks.unwrap_or(&whole);

    // §10.6's subblock split is over the *compression*, so each pair is read on its own into
    // its own run of the destination.
    let mut in_at = 0usize;
    let mut out_at = 0usize;
    for (index, pair) in list.iter().enumerate() {
        let compressed = usize::try_from(pair.0)
            .map_err(|_| Error::malformed("subblocks: a compressed length does not fit usize"))?;
        let uncompressed = usize::try_from(pair.1).map_err(|_| {
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
        decode_pair(
            &mut codecs,
            classify(*pair, reading),
            src,
            compression.codec,
            dst,
            limits,
        )?;
        in_at += compressed;
        out_at += uncompressed;
    }
    Ok(())
}

/// Decode one **resident** pair — its stored bytes in `stored`, its destination in `dst` — as
/// `kind` says.
///
/// Both walks decode a resident pair here and nowhere else: the materializing walk every pair,
/// and the streaming walk each pair it holds whole, a bare LZ4 subblock or a
/// [`PairKind::Probe`]. The rule for an attempt that fails therefore has one home.
///
/// A [`PairKind::Probe`] falls back to its stored bytes when the codec refuses them, and on
/// nothing else. The attempt can also trip a cap — a zstd frame declaring a window above
/// `zstd_window_bytes` — and that is reported: the pair may be valid compressed data the cap
/// refuses, and reading it as pixels instead would fabricate them.
pub(crate) fn decode_pair(
    codecs: &mut Codecs,
    kind: PairKind,
    stored: &[u8],
    codec: Codec,
    dst: &mut [u8],
    limits: &Limits,
) -> Result<()> {
    match kind {
        PairKind::Raw => {
            copy_stored(stored, dst);
            Ok(())
        }
        PairKind::Compressed => decompress_one(codecs, stored, codec, dst, limits)
            .map_err(|fault| fault.into_error(codec, dst.len())),
        PairKind::Probe => match decompress_one(codecs, stored, codec, dst, limits) {
            Ok(()) => Ok(()),
            // Whatever the codec wrote before it refused is overwritten whole.
            Err(Fault::Corrupt(_)) => {
                copy_stored(stored, dst);
                Ok(())
            }
            Err(Fault::Refused(error)) => Err(error),
        },
    }
}

/// An uncompressed subblock's bytes are its data (§10.6.1).
///
/// [`classify`] reads a pair as stored bytes only when its two lengths are equal, and both
/// slices are cut from those lengths, so a mismatch here is the decoder contradicting itself
/// rather than a file defect — the class § Errors has no honest slot for, and the crate
/// panics on. `copy_from_slice` is that panic.
fn copy_stored(stored: &[u8], dst: &mut [u8]) {
    dst.copy_from_slice(stored);
}

/// The framed codecs' state, carried across a block's subblocks **by both walks, whatever
/// each subblock is**.
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
/// **An uncompressed subblock never touches it.** §10.6.1's uncompressed subblocks sit among
/// compressed ones as often as a file likes, so a raw pair that dropped or rebuilt this state
/// would pay the construction cost once per alternation — the same defect by another door.
/// Owning the state apart from how the *current* subblock is delivered makes surviving them a
/// property of the structure rather than of each arm remembering to.
///
/// LZ4 is absent because it has no state to carry — `lz4_flex::block::decompress_into`
/// allocates no codec and writes straight into the destination.
#[derive(Default)]
pub(crate) struct Codecs {
    zlib: Option<flate2::Decompress>,
    zstd: Option<Box<ruzstd::decoding::FrameDecoder>>,
}

impl Codecs {
    fn zlib(&mut self) -> &mut flate2::Decompress {
        self.zlib
            .get_or_insert_with(|| flate2::Decompress::new(true))
    }

    fn zstd(&mut self) -> &mut ruzstd::decoding::FrameDecoder {
        self.zstd
            .get_or_insert_with(|| Box::new(ruzstd::decoding::FrameDecoder::new()))
    }
}

/// Why a resident subblock did not decode, kept as a value until it is reported.
///
/// **Nothing on the failing path allocates until [`Fault::into_error`] runs, and that runs
/// only where the failure is returned.** A [`PairKind::Probe`] the codec refuses is not an
/// error at all — it falls back to its stored bytes — and a subblock list holds up to
/// `Subblock count` of them. A message formatted per refusal, or a codec error boxed into an
/// `io::Error` to pass through a `Read` adapter, is a per-occurrence allocation multiplied by
/// a cap (`docs/intentional-patterns.md`), paid on every refusal and thrown away. Measured on
/// a zlib block alternating uncompressed and compressed subblocks under `TryDecompress`, split
/// 8 ways and then 4096: the message and the box together took the streaming decode from
/// 306 458 bytes to 711 104, 2.32×, and the message alone to 1.43× — under the 2× each shape in
/// `tests/peak_memory.rs` asserts, which is why that file also asserts what the option costs
/// over the default on the same block, which a refusal allocating anything at all exceeds.
enum Fault {
    /// The stored bytes are not a stream of the declared codec that ends after exactly the
    /// subblock's uncompressed length. `Malformed` when reported, and the one failure a
    /// [`PairKind::Probe`] falls back on.
    Corrupt(Corrupt),
    /// A cap the attempt tripped, reported as it stands.
    Refused(Error),
}

/// How a codec refused a resident subblock — each variant holds what its message needs and
/// nothing that cost an allocation to build.
enum Corrupt {
    Lz4(lz4_flex::block::DecompressError),
    Zlib(flate2::DecompressError),
    ZstdHeader(ruzstd::decoding::errors::FrameDecoderError),
    Zstd(ruzstd::decoding::errors::FrameDecoderError),
    /// The stream ended after producing this many bytes, short of the destination.
    Short(usize),
    /// The stream had more to give past the destination.
    Long,
}

impl From<Corrupt> for Fault {
    fn from(corrupt: Corrupt) -> Fault {
        Fault::Corrupt(corrupt)
    }
}

impl Fault {
    /// The error to report, for a subblock of `codec` owed `len` bytes.
    fn into_error(self, codec: Codec, len: usize) -> Error {
        let name = match codec {
            Codec::Lz4 | Codec::Lz4Hc => "LZ4",
            Codec::Zlib => "zlib",
            Codec::Zstd => "zstd",
        };
        match self {
            Fault::Refused(error) => error,
            Fault::Corrupt(Corrupt::Lz4(e)) => {
                Error::malformed(format!("LZ4 block does not decompress: {e}"))
            }
            Fault::Corrupt(Corrupt::Zlib(e)) => {
                Error::malformed(format!("zlib block does not decompress: {e}"))
            }
            Fault::Corrupt(Corrupt::ZstdHeader(e)) => {
                Error::malformed(format!("zstd frame header is not readable: {e}"))
            }
            Fault::Corrupt(Corrupt::Zstd(e)) => {
                Error::malformed(format!("zstd block does not decompress: {e}"))
            }
            Fault::Corrupt(Corrupt::Short(produced)) => Error::malformed(format!(
                "{name} block inflates to {produced} bytes where the geometry implies {len}"
            )),
            Fault::Corrupt(Corrupt::Long) => Error::malformed(format!(
                "{name} block inflates past the {len} bytes the geometry implies"
            )),
        }
    }
}

fn decompress_one(
    codecs: &mut Codecs,
    stored: &[u8],
    codec: Codec,
    out: &mut [u8],
    limits: &Limits,
) -> std::result::Result<(), Fault> {
    match codec {
        // A bare LZ4 block: no frame header, decompresses only as a whole, and the output
        // length is known from the geometry rather than read from the stream.
        Codec::Lz4 | Codec::Lz4Hc => {
            let written = lz4_flex::block::decompress_into(stored, out).map_err(Corrupt::Lz4)?;
            if written != out.len() {
                return Err(Corrupt::Short(written).into());
            }
            Ok(())
        }
        // zlib-wrapped, not raw deflate: feeding these bytes to an inflate-raw reader
        // type-checks and then fails on every block.
        Codec::Zlib => {
            let d = codecs.zlib();
            d.reset(true);
            let mut inflate = Inflate {
                d,
                input: stored,
                done: false,
            };
            exact(|buf| inflate.pull(buf).map_err(Corrupt::Zlib), out)
        }
        // Framed. The frame header declares a window size and a zstd decoder allocates that
        // window before producing a byte -- the precise shape of "an allocation sized from an
        // unvalidated declared size" -- so the cap is set on the decoder before the frame is
        // read, and the read checks it before the allocation.
        Codec::Zstd => {
            let fd = codecs.zstd();
            fd.set_max_window_size(limits.zstd_window_bytes);
            let mut input = stored;
            fd.reset(&mut input)
                .map_err(|e| zstd_header_fault(e, limits))?;
            exact(
                |buf| zstd_pull(fd, &mut input, buf).map_err(Corrupt::Zstd),
                out,
            )
        }
    }
}

/// Fill `out` exactly from `pull`, then require the stream to be over.
///
/// `Read::read_exact` and a one-byte `read` past the destination, spelled out, because a
/// `Read` adapter has to wrap the codec's refusal in an `io::Error` — a box per refusal, which
/// [`Fault`] says why this path cannot pay. A pull returning zero means the stream has nothing
/// further to give: short of the destination that is a truncation, and past it an end.
fn exact(
    mut pull: impl FnMut(&mut [u8]) -> std::result::Result<usize, Corrupt>,
    out: &mut [u8],
) -> std::result::Result<(), Fault> {
    let mut filled = 0usize;
    while filled < out.len() {
        match pull(&mut out[filled..])? {
            0 => return Err(Corrupt::Short(filled).into()),
            n => filled += n,
        }
    }
    let mut extra = [0u8; 1];
    match pull(&mut extra)? {
        0 => Ok(()),
        _ => Err(Corrupt::Long.into()),
    }
}

/// One subblock's stored bytes, driving a decompressor the whole block shares.
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
    /// Set once the stream has ended, so that the byte [`exact`] asks for past the
    /// destination reaches a spent decompressor rather than an undefined one.
    done: bool,
}

impl Inflate<'_> {
    fn pull(&mut self, out: &mut [u8]) -> std::result::Result<usize, flate2::DecompressError> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        let before_in = self.d.total_in();
        let before_out = self.d.total_out();
        let status = self
            .d
            .decompress(self.input, out, flate2::FlushDecompress::Finish)?;
        let consumed = (self.d.total_in() - before_in) as usize;
        let produced = (self.d.total_out() - before_out) as usize;
        self.input = &self.input[consumed..];
        // A finished stream and an iteration that moved neither counter both mean there is
        // nothing further to give, which a pull says by returning zero: [`exact`] turns a short
        // pull into the truncation it is, and its one-byte pull past the destination catches a
        // block that inflates too far. Trailing bytes after the stream's end are ignored rather
        // than refused, which is the treatment a reader over the same decompressor gives them.
        self.done = matches!(status, flate2::Status::StreamEnd) || (produced == 0 && consumed == 0);
        Ok(produced)
    }
}

/// Up to `buf.len()` bytes of a resident zstd frame — `ruzstd`'s `StreamingDecoder::read`,
/// driven directly so that a refusal stays the decoder's own error rather than a boxed
/// `io::Error`.
///
/// `UptoBytes` does not promise to reach its target, and while a frame is unfinished the
/// decoder retains a window's worth of output it will not hand over, so it is asked again
/// until enough is collectable or the frame is done.
fn zstd_pull(
    fd: &mut ruzstd::decoding::FrameDecoder,
    input: &mut &[u8],
    buf: &mut [u8],
) -> std::result::Result<usize, ruzstd::decoding::errors::FrameDecoderError> {
    use ruzstd::decoding::BlockDecodingStrategy;
    use ruzstd::decoding::errors::FrameDecoderError;
    if fd.is_finished() && fd.can_collect() == 0 {
        return Ok(0);
    }
    while fd.can_collect() < buf.len() && !fd.is_finished() {
        let more = buf.len() - fd.can_collect();
        fd.decode_blocks(&mut *input, BlockDecodingStrategy::UptoBytes(more))?;
    }
    fd.read(buf)
        .map_err(FrameDecoderError::FailedToDrainDecodebuffer)
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
///
/// **A failed read keeps the source's error, and the caller reports that one.** The codec sees
/// only an `io::Error`, and what it hands back is its own complaint about its input: a broken
/// pipe mid-block comes out as "zlib block does not decompress", which § Errors would class as
/// a damaged file — skip and walk on — when the move a failing source calls for is abort.
/// Neither the codec's error nor the `io::ErrorKind` can tell the two apart, so the reader
/// records what the source raised and [`BlockReader::failure_or`] puts it back. Bytes read for
/// no codec — an uncompressed subblock, or a subblock held whole — go through
/// [`BlockReader::copy`], which reports the source's error directly.
struct BlockReader<'a, S: Source> {
    src: &'a mut S,
    limits: &'a Limits,
    /// Stored bytes of the current subblock not yet handed to the codec.
    left: &'a mut u64,
    /// Where the next of them sits in the file.
    at: &'a mut u64,
    /// The source's own error, when a read failed because the source did.
    failure: Option<Error>,
}

impl<'a, S: Source> BlockReader<'a, S> {
    fn new(src: &'a mut S, limits: &'a Limits, left: &'a mut u64, at: &'a mut u64) -> Self {
        BlockReader {
            src,
            limits,
            left,
            at,
            failure: None,
        }
    }

    /// The error to report for a codec failure: the source's, when a read through this reader
    /// failed, since that is what the codec was reacting to, and `codec_error` otherwise.
    fn failure_or(&mut self, codec_error: Error) -> Error {
        self.failure.take().unwrap_or(codec_error)
    }

    fn read_source(&mut self, out: &mut [u8]) -> Result<()> {
        if self.src.position() != *self.at {
            self.src.seek_to(*self.at, self.limits)?;
        }
        self.src.read_exact(out)
    }

    /// Fill `out` with the next stored bytes, which the caller has already bounded by what is
    /// left of the subblock.
    fn copy(&mut self, out: &mut [u8]) -> Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        debug_assert!(
            out.len() as u64 <= *self.left,
            "a copy past the subblock's end"
        );
        self.read_source(out)?;
        *self.left -= out.len() as u64;
        *self.at += out.len() as u64;
        Ok(())
    }
}

impl<S: Source> Read for BlockReader<'_, S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if *self.left == 0 || out.is_empty() {
            return Ok(0);
        }
        let want = (*self.left).min(out.len() as u64) as usize;
        if let Err(e) = self.read_source(&mut out[..want]) {
            // The text the codec wraps; the error itself is what `failure_or` returns.
            let relayed = std::io::Error::other(e.to_string());
            self.failure = Some(e);
            return Err(relayed);
        }
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

/// What [`Stream::open`] opens: the block's codec, how [`classify`] reads this pair, and how
/// many bytes the pair owes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opening {
    pub(crate) codec: Codec,
    pub(crate) kind: PairKind,
    pub(crate) uncompressed: u64,
}

/// How the streaming walk reads a compressed block, one subblock at a time.
///
/// Two things live here and are kept apart on purpose: the codec **state**, in [`Codecs`],
/// which lasts the whole block, and how the **current** subblock is delivered, in
/// [`Current`], which each [`Stream::open`] replaces. An uncompressed subblock changes the
/// second and never the first, so a block alternating raw and compressed subblocks resets one
/// decoder at each compressed boundary rather than building one, which is what
/// [`Codecs`] is for.
///
/// An uncompressed block has no subblock walk at all and never opens one: the decoder routes it
/// on its plan, reading rows at computed offsets.
#[derive(Default)]
pub(crate) struct Stream {
    codecs: Codecs,
    zlib: ZlibInput,
    zstd: ZstdOutput,
    current: Current,
}

/// How the current subblock's bytes are delivered.
#[derive(Default)]
enum Current {
    /// Nothing is open: before the first [`Stream::open`], and after one that failed. It
    /// delivers nothing, so a read through it is a truncation rather than a fabricated row.
    #[default]
    Ended,
    /// An uncompressed subblock (§10.6.1), read straight from the source into the caller's
    /// buffer.
    Raw,
    /// zlib-wrapped, decompressing incrementally.
    Zlib,
    /// Framed zstd, likewise incremental, with the declared window capped before the frame
    /// header is read.
    Zstd,
    /// One subblock decoded whole by [`decode_pair`], and how much of it has been handed over.
    ///
    /// A bare LZ4 block has no framing, so nothing smaller than a whole subblock can be
    /// produced; a [`PairKind::Probe`] of any codec has to be resident for its attempt. Both
    /// are exactly the `Block` floor `granularity()` reports, and exactly the peak memory
    /// § Streaming promises for it: one subblock, compressed and decompressed, rather than the
    /// whole image.
    Held { buf: Vec<u8>, at: usize },
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.current {
            Current::Ended => "Ended",
            Current::Raw => "Raw",
            Current::Zlib => "Zlib",
            Current::Zstd => "Zstd",
            Current::Held { .. } => "Held",
        })
    }
}

impl Stream {
    /// Open the subblock the cursors describe, as `opening.kind` says, **reusing this stream's
    /// codec state**.
    ///
    /// §10.6 splits the compression, so this runs once per subblock and the number of times is
    /// `Subblock count` rather than anything the file's length bounds. Both framed codecs
    /// therefore carry one state across the whole block and reset it at each compressed
    /// boundary — a `flate2::Decompress` costs about 43 KB to construct and a
    /// `ruzstd::FrameDecoder` about 9.5 KB, and constructing either per subblock multiplies a
    /// fixed cost by a cap. `Decompress::reset` and `FrameDecoder::reset` both keep the buffers
    /// and reset the state, which is the same decode from a clean stream position.
    ///
    /// The peak is unchanged and § Streaming's promise of one subblock at a time still holds:
    /// reuse holds strictly less than construction did, and a held subblock's buffers — the
    /// whole-subblock ones the promise is about — are dropped before the incoming subblock's
    /// are sized.
    pub(crate) fn open<S: Source>(
        &mut self,
        opening: Opening,
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<()> {
        // Drop the outgoing subblock's buffers *before* sizing the incoming one's: holding two
        // held subblocks at once is twice what § Streaming's peak-memory row promises. It also
        // leaves the stream delivering nothing if this open fails.
        self.current = Current::Ended;
        let Opening {
            codec,
            kind,
            uncompressed,
        } = opening;
        self.current = match (kind, codec) {
            // Read straight from the source, with no buffer and no codec.
            (PairKind::Raw, _) => Current::Raw,
            (PairKind::Compressed, Codec::Zlib) => {
                self.codecs.zlib().reset(true);
                self.zlib.prepare(input_window(*left));
                Current::Zlib
            }
            (PairKind::Compressed, Codec::Zstd) => {
                let fd = self.codecs.zstd();
                fd.set_max_window_size(limits.zstd_window_bytes);
                self.zstd = ZstdOutput::default();
                let mut reader = BlockReader::new(src, limits, left, at);
                // A zstd decoder allocates the window the frame header declares before
                // producing a byte -- the precise shape of an allocation sized from an
                // unvalidated declared size -- so the cap is enforced here, before the
                // allocation, and a trip is LimitExceeded like any other cap.
                fd.reset(&mut reader)
                    .map_err(|e| reader.failure_or(zstd_header_error(e, limits)))?;
                Current::Zstd
            }
            (PairKind::Compressed, Codec::Lz4 | Codec::Lz4Hc) | (PairKind::Probe, _) => {
                // Both allocations are sized from a declared length, so both are capped before
                // they are made. `materialized_bytes` is the right cap: this is the one buffer
                // a `Block`-granularity decode materializes.
                let compressed_len = check_materialized(*left, limits, "a held subblock's stored")?;
                let out_len = check_materialized(uncompressed, limits, "a held subblock's")?;
                let mut stored = vec![0u8; compressed_len];
                BlockReader::new(src, limits, left, at).copy(&mut stored)?;
                let mut buf = vec![0u8; out_len];
                decode_pair(&mut self.codecs, kind, &stored, codec, &mut buf, limits)?;
                Current::Held { buf, at: 0 }
            }
        };
        Ok(())
    }

    /// Fill `out` exactly, pulling stored bytes from the source as the subblock asks for them.
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

    /// Require the subblock to be at its end.
    ///
    /// This is what refuses a decompression bomb: a block that inflates to exactly the
    /// geometry-implied size and then still has more to give is not the block the header
    /// described. It is enforced per subblock, so a bomb hidden in any one of them is caught
    /// at its own boundary. An uncompressed subblock ends where its length says, having
    /// delivered all of it, so the probe finds nothing there.
    pub(crate) fn require_end<S: Source>(
        &mut self,
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<()> {
        let mut probe = [0u8; 1];
        if self.fill_upto(&mut probe, src, left, at, limits)? > 0 {
            return Err(Error::malformed(
                "compressed block inflates past the size the geometry implies",
            ));
        }
        Ok(())
    }

    /// Produce up to `out.len()` bytes, returning how many. Short only at the subblock's end.
    fn fill_upto<S: Source>(
        &mut self,
        out: &mut [u8],
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<usize> {
        match &mut self.current {
            Current::Ended => Ok(0),
            Current::Raw => {
                let take = usize::try_from(*left).map_or(out.len(), |left| left.min(out.len()));
                BlockReader::new(src, limits, left, at).copy(&mut out[..take])?;
                Ok(take)
            }
            Current::Zlib => self
                .zlib
                .fill_upto(self.codecs.zlib(), out, src, left, at, limits),
            Current::Zstd => self
                .zstd
                .fill_upto(self.codecs.zstd(), out, src, left, at, limits),
            Current::Held { buf, at: held_at } => {
                let take = (buf.len() - *held_at).min(out.len());
                out[..take].copy_from_slice(&buf[*held_at..*held_at + take]);
                *held_at += take;
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
/// raised limits, which can never succeed. It is also the line a [`PairKind::Probe`] falls back
/// on: a bad magic is a subblock that is not a zstd frame, and a window above the cap may be one.
fn zstd_header_fault(e: ruzstd::decoding::errors::FrameDecoderError, limits: &Limits) -> Fault {
    use ruzstd::decoding::errors::{FrameDecoderError, FrameHeaderError};
    match &e {
        FrameDecoderError::WindowSizeTooBig { .. }
        | FrameDecoderError::FrameHeaderError(FrameHeaderError::WindowTooBig { .. })
        | FrameDecoderError::FailedToInitialize(FrameHeaderError::WindowTooBig { .. }) => {
            Fault::Refused(Error::limit(format!(
                "zstd decoder window: the frame header's declared window is above the {} byte \
                 cap ({e})",
                limits.zstd_window_bytes
            )))
        }
        _ => Fault::Corrupt(Corrupt::ZstdHeader(e)),
    }
}

/// [`zstd_header_fault`], reported.
fn zstd_header_error(e: ruzstd::decoding::errors::FrameDecoderError, limits: &Limits) -> Error {
    zstd_header_fault(e, limits).into_error(Codec::Zstd, 0)
}

/// zlib's input window, which the stream's one `flate2::Decompress` is fed from.
#[derive(Default)]
struct ZlibInput {
    buf: Vec<u8>,
    at: usize,
    end: usize,
}

impl ZlibInput {
    /// Empty the window for a subblock that wants `want` bytes of it.
    ///
    /// The window only ever grows, and it grows to the subblock that asked for it, so the sum
    /// over a block is bounded by the block's stored bytes. The old buffer is released before
    /// the larger one is sized, since nothing in it survives a reset.
    fn prepare(&mut self, want: usize) {
        self.at = 0;
        self.end = 0;
        if self.buf.len() < want {
            self.buf = Vec::new();
            self.buf = vec![0u8; want];
        }
    }

    fn fill_upto<S: Source>(
        &mut self,
        d: &mut flate2::Decompress,
        out: &mut [u8],
        src: &mut S,
        left: &mut u64,
        at: &mut u64,
        limits: &Limits,
    ) -> Result<usize> {
        let mut filled = 0usize;
        while filled < out.len() {
            if self.at >= self.end {
                let mut reader = BlockReader::new(src, limits, left, at);
                let got = reader.read(&mut self.buf).map_err(|e| {
                    reader.failure_or(Error::malformed(format!("reading a compressed block: {e}")))
                })?;
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

            let before_in = d.total_in();
            let before_out = d.total_out();
            let status = d
                .decompress(&self.buf[self.at..self.end], &mut out[filled..], flush)
                .map_err(|e| Error::malformed(format!("zlib block does not decompress: {e}")))?;
            let consumed = (d.total_in() - before_in) as usize;
            let produced = (d.total_out() - before_out) as usize;
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

/// What the stream's one zstd `FrameDecoder` has produced and not yet handed over.
#[derive(Default)]
struct ZstdOutput {
    pending: Vec<u8>,
    at: usize,
}

impl ZstdOutput {
    fn fill_upto<S: Source>(
        &mut self,
        fd: &mut ruzstd::decoding::FrameDecoder,
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
            if fd.is_finished() && fd.can_collect() == 0 {
                return Ok(filled);
            }

            // Let the decoder run even once the stored block is spent. While a frame is
            // unfinished, ruzstd **retains window_size bytes** and will not hand them over, so
            // bailing out the moment the input ran dry strands the tail of every image whose
            // last bytes sit inside that window. Reading zero is how the decoder learns the
            // frame ended, and finishing is what releases the retained bytes.
            let want = out.len() - filled;
            let mut reader = BlockReader::new(src, limits, left, at);
            fd.decode_blocks(
                &mut reader,
                ruzstd::decoding::BlockDecodingStrategy::UptoBytes(want),
            )
            .map_err(|e| {
                reader.failure_or(Error::malformed(format!(
                    "zstd block does not decompress: {e}"
                )))
            })?;

            match fd.collect() {
                Some(v) if !v.is_empty() => {
                    self.pending = v;
                    self.at = 0;
                }
                _ if *left == 0 || fd.is_finished() => return Ok(filled),
                _ => {}
            }
        }
        Ok(filled)
    }
}
