//! The XISF decoder: the preamble, the image-occurrence walk, and block delivery.

use crate::error::{Error, Result};
use crate::header::{DeclineClass, DeclineReason, Granularity, Header, PixelStorage};
use crate::limits::{Limits, narrow};
use crate::reader::{ChunkMeta, PixelPlan};
use crate::samples::{SampleSlice, Samples, slice_samples};
use crate::source::Source;
use crate::xisf::block::{self, Codec, Location, unshuffled_byte};
use crate::xisf::codec::{self, Stream};
use crate::xisf::image::{BlockPlan, Occurrence, walk_occurrences};
use crate::xisf::xml;

/// The signature plus the header-length field plus the reserved field.
pub(crate) const PREAMBLE: u64 = 16;

/// §9.2's own validity check: the minimum length of an empty header from `<?xml…` to
/// `</xisf>`.
const MIN_HEADER: u64 = 65;

/// How much of the declared header is read per call, so a declared length never becomes an
/// allocation.
const HEADER_CHUNK: usize = 64 * 1024;

/// Pixel delivery state for the position the reader sits on.
#[derive(Debug)]
struct PixelState {
    plan: BlockPlan,
    selected: Option<u32>,
    /// How many chunks this decode delivers, and how many it has.
    chunks_total: u64,
    chunks_done: u64,
    /// The decompressed, still-shuffled block, when the whole thing had to be materialized.
    whole: Option<std::sync::Arc<Vec<u8>>>,
    /// One input row of stored bytes, for the streaming path, and which row it holds.
    raw: Vec<u8>,
    raw_row: Option<u64>,
    scratch: Samples,
    /// How the streaming path pulls rows.
    stream: Stream,
    /// The next input row a *compressed* stream will yield. A stream cannot seek, so reaching
    /// a later row means reading and discarding the ones before it.
    stream_row: u64,
    /// Stored bytes of the **current subblock** not yet handed to the decoder, and where the
    /// next of them sits in the file.
    stored_left: u64,
    stored_at: u64,
    /// Where the whole stored block begins, so a subblock boundary is computed from the
    /// declared lengths rather than from how much the codec happened to consume.
    block_start: u64,
    /// Which subblock the stream is inside, and how much of its output is still owed.
    ///
    /// §10.6 splits the *compression*, so each subblock is an independently compressed stream
    /// and the codec has to be restarted at every boundary. A single stream opened over the
    /// whole stored range ends after the first subblock and reports a truncated block. A block
    /// with no `subblocks` attribute is one subblock spanning the whole of it.
    subblock_index: usize,
    subblock_uncompressed_left: u64,
}

/// A monolithic XISF unit, walked one image occurrence at a time.
#[derive(Debug)]
pub(crate) struct Decoder {
    occurrences: Vec<Occurrence>,
    /// The occurrence the reader sits on; `None` until the first advance.
    current: Option<usize>,
    /// The next occurrence to advance to.
    next: usize,
    pixels: Option<PixelState>,
    /// Set once the current image's samples have been read, which is what makes a re-read
    /// need a seekable source.
    pixels_read: bool,
}

impl Decoder {
    /// Read the preamble and the XML header, and stop. No image is selected.
    pub(crate) fn new<S: Source>(src: &mut S, limits: &Limits) -> Result<Decoder> {
        let mut rest = [0u8; 8];
        src.read_exact(&mut rest)?;
        let declared = u64::from(u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]));
        // §9.2's four reserved bytes "shall be zero" binds encoders and places no obligation
        // on readers; the specification's own validity check inspects only the signature and
        // the minimum length. Ignoring them is plain conformance rather than leniency.

        if declared < MIN_HEADER {
            return Err(Error::malformed(format!(
                "XISF header length {declared} is below §9.2's {MIN_HEADER}-byte minimum"
            )));
        }
        // Rejected above the cap **before** the read. The XML header is the one deliberate
        // exception to "geometry is the ceiling", having no geometry to be checked against,
        // so this cap plus the incremental read is what keeps a declared size from ever
        // becoming an allocation.
        if declared > limits.xml_header_bytes {
            return Err(Error::limit(format!(
                "XML header length: the unit declares {declared} bytes, above the {} byte cap",
                limits.xml_header_bytes
            )));
        }

        let header_bytes = read_incrementally(src, declared)?;
        let doc = xml::parse(&header_bytes, limits)?;

        match doc.attr(doc.root, "version") {
            Some("1.0") => {}
            Some(other) => {
                return Err(Error::unsupported(format!(
                    "XISF root version {other:?}: this version reads 1.0"
                )));
            }
            None => {
                return Err(Error::malformed(
                    "the XISF root element carries no version attribute, which §9.5 makes \
                     mandatory",
                ));
            }
        }

        let mut occurrences = walk_occurrences(&doc, limits)?;
        refuse_attachments_inside_the_header(&mut occurrences, PREAMBLE + declared);
        verify_materialized_blocks(&occurrences)?;

        Ok(Decoder {
            occurrences,
            current: None,
            next: 0,
            pixels: None,
            pixels_read: false,
        })
    }

    pub(crate) fn header(&self) -> Option<&Header> {
        self.current
            .and_then(|i| self.occurrences.get(i))
            .map(|o| &o.header)
    }

    /// The one `String` allocation the shared `bounds` text costs, made where the caller asks
    /// for it rather than once per occurrence.
    pub(crate) fn declared_bounds_text(&self) -> Option<String> {
        self.current
            .and_then(|i| self.occurrences.get(i))
            .and_then(|o| o.declared_bounds.as_deref())
            .map(str::to_owned)
    }

    /// Advance to the next image occurrence.
    ///
    /// A root-level `Reference` to an `Image` **is** an occurrence: §11.13's second worked
    /// example rewrites four identical images as one `<Image uid="…">` plus three bare
    /// root-level references, which it calls achieving "the same result … in a much cleaner
    /// way". A decoder walking only `Image` elements would report one image on that
    /// conforming file, silently. The walk was done at construction, the whole header being
    /// buffered anyway.
    pub(crate) fn next_image<S: Source>(&mut self, _src: &mut S, _limits: &Limits) -> Result<bool> {
        if self.next >= self.occurrences.len() {
            self.current = None;
            return Ok(false);
        }
        self.current = Some(self.next);
        self.next += 1;
        self.pixels = None;
        self.pixels_read = false;
        Ok(true)
    }

    /// XISF blocks are located by declared offset rather than by walking past them, so a
    /// declined or abandoned image never blocks the next one and there is nothing to skip.
    pub(crate) fn abandon<S: Source>(&mut self, _src: &mut S, _limits: &Limits) -> Result<()> {
        self.pixels = None;
        Ok(())
    }

    pub(crate) fn begin_pixels<S: Source>(
        &mut self,
        src: &mut S,
        limits: &Limits,
        plan: &PixelPlan,
    ) -> Result<()> {
        let occurrence = self
            .current
            .and_then(|i| self.occurrences.get(i))
            .ok_or_else(|| Error::invalid_request("no image is selected"))?;
        let block = occurrence
            .plan
            .clone()
            .ok_or_else(|| Error::invalid_request("this position has no pixel data to read"))?;

        if self.pixels_read && !src.is_seekable() {
            return Err(Error::unsupported(
                "re-reading an image's pixels needs a seekable source; this one is forward-only",
            ));
        }

        let g = block.geometry;
        let sample_bytes = u64::from(block.format.bytes());

        if let Location::Attachment { position, size } = block.location {
            // On a source of known length an uncompressed image cannot need more stored bytes
            // than the file contains. This runs here rather than during header decode, and
            // that placement is load-bearing: a size-capped *prefix* handed to
            // `Reader::seekable` has exactly this shape, and refusing it would break
            // header-only decode.
            if let Some(len) = src.length()
                && position.saturating_add(size) > len
            {
                return Err(Error::malformed(format!(
                    "attachment at {position} for {size} bytes runs past the {len}-byte source"
                )));
            }
            // A declared `attachment:pos:size` becomes an allocation for any codec that must
            // be fully resident, and a compressed block may legitimately exceed its
            // uncompressed size, so the geometry cross-check alone cannot bound it. On a
            // seekable source the file length bounds it incidentally; over a pipe there is no
            // file length at all.
            let stored_cap = block
                .implied_bytes
                .saturating_mul(limits.stored_block_multiple)
                .max(limits.stored_block_floor_bytes);
            if size > stored_cap {
                return Err(Error::limit(format!(
                    "stored block bytes: the block declares {size} bytes, above the \
                     {stored_cap} byte cap"
                )));
            }
            if block.compression.is_none() && size != block.implied_bytes {
                return Err(Error::malformed(format!(
                    "uncompressed block declares {size} bytes where the geometry implies {}",
                    block.implied_bytes
                )));
            }
        }

        // A declared uncompressed size that disagrees with the geometry-implied size is
        // refused *before* any buffer is allocated, which makes decompression bombs
        // structurally impossible rather than merely caught.
        if let Some(c) = &block.compression
            && c.uncompressed_size != block.implied_bytes
        {
            return Err(Error::malformed(format!(
                "compression declares an uncompressed size of {} where the geometry implies {}",
                c.uncompressed_size, block.implied_bytes
            )));
        }

        // The row-shaped staging buffer is one of the buffers `Materialized bytes` governs,
        // measured at its own size before it is sized. With `Normal` storage every input row
        // yields samples for all channels, so the cost is one row-sized staging buffer rather
        // than an image-sized one — which is why interleaving changes no granularity.
        let row_samples = u64::from(g.width)
            * if block.storage == PixelStorage::Normal {
                u64::from(g.channels)
            } else {
                1
            };
        // Checked for the reason `output_bytes` is: the pixel-phase gate rules out a geometry
        // whose byte extent overflows, so this cannot fail today, and this is exactly the site
        // that would size a staging buffer from a wrapped remainder if that ever changed.
        let row_bytes = row_samples.checked_mul(sample_bytes).ok_or_else(|| {
            Error::limit(format!(
                "materialized bytes: a staging row of {row_samples} samples at {sample_bytes} \
                 bytes each overflows 64-bit arithmetic"
            ))
        })?;
        if row_bytes > limits.materialized_bytes {
            return Err(Error::limit(format!(
                "materialized bytes: one staging row is {row_bytes} bytes, above the {} cap",
                limits.materialized_bytes
            )));
        }

        let streaming = is_streamable(&block);
        let mut stream = Stream::Plain;
        let mut stored_left = 0u64;
        let mut stored_at = 0u64;
        let mut block_start = 0u64;
        let mut subblock_uncompressed_left = 0u64;
        let whole = if streaming {
            // `is_streamable` returns false for every location but `Attachment`, so this is
            // a decoder-internal invariant rather than anything a file states — the class the
            // crate panics on rather than misreporting as a bad file (§ Errors).
            let Location::Attachment { position, size } = block.location else {
                unreachable!(
                    "internal invariant violated: a streaming decode was started on a non-attachment location"
                );
            };
            src.seek_to(position, limits)?;
            stored_at = position;
            block_start = position;
            // A block with no `subblocks` attribute is one subblock spanning the whole of it.
            let (first_compressed, first_uncompressed) = match block.subblocks.as_deref() {
                Some([first, ..]) => *first,
                _ => (size, block.implied_bytes),
            };
            stored_left = first_compressed;
            subblock_uncompressed_left = first_uncompressed;
            if let Some(c) = &block.compression {
                // zstd parses its frame header here, which is also where the declared window
                // is checked -- before the window is allocated.
                stream.open(
                    c.codec,
                    src,
                    &mut stored_left,
                    &mut stored_at,
                    subblock_uncompressed_left,
                    limits,
                )?;
            }
            None
        } else {
            if block.implied_bytes > limits.materialized_bytes {
                return Err(Error::limit(format!(
                    "materialized bytes: this block must be held whole at {} bytes, above the \
                     {} byte cap",
                    block.implied_bytes, limits.materialized_bytes
                )));
            }
            Some(materialize(&block, src, limits)?)
        };

        // One chunk per (channel, row) pair the decode delivers.
        let chunks_total = match plan.selected_channel {
            Some(_) => u64::from(g.height),
            None => u64::from(g.height) * u64::from(g.channels),
        };

        self.pixels = Some(PixelState {
            selected: plan.selected_channel,
            chunks_total,
            chunks_done: 0,
            whole,
            raw: vec![0u8; narrow(row_bytes, "row staging buffer")?],
            raw_row: None,
            stream,
            stream_row: 0,
            stored_left,
            stored_at,
            block_start,
            subblock_index: 0,
            subblock_uncompressed_left,
            scratch: Samples::zeroed(block.format, narrow(u64::from(g.width), "chunk")?),
            plan: block,
        });
        self.pixels_read = false;
        Ok(())
    }

    pub(crate) fn next_chunk<S: Source>(
        &mut self,
        src: &mut S,
        limits: &Limits,
    ) -> Result<Option<ChunkMeta>> {
        let Some(p) = self.pixels.as_mut() else {
            return Ok(None);
        };
        if p.chunks_done >= p.chunks_total {
            self.pixels_read = true;
            return Ok(None);
        }

        let g = p.plan.geometry;
        let width = u64::from(g.width);
        let height = u64::from(g.height);
        let channels = u64::from(g.channels);
        let i = p.chunks_done;

        // The cursor: which file channel this chunk belongs to, which row of the image it is,
        // and which *input* row carries it. With `Planar` storage a chunk is exactly one
        // input row; with `Normal` storage one input row yields a chunk for every channel.
        let (channel, row, input_row) = match (p.plan.storage, p.selected) {
            (PixelStorage::Planar, Some(k)) => (u64::from(k), i, u64::from(k) * height + i),
            (PixelStorage::Planar, None) => (i / height, i % height, i),
            (PixelStorage::Normal, Some(k)) => (u64::from(k), i, i),
            (PixelStorage::Normal, None) => (i % channels, i / channels, i / channels),
        };

        // Destination coordinates: offsets into the buffer the caller supplied, so assembling
        // a buffer from chunks is a copy at the stated offset with no recalculation. Under
        // `select_channel` the destination has one plane while the file still has many.
        let dest_row = if p.selected.is_some() {
            row
        } else {
            channel * height + row
        };
        let dest_start = dest_row * width;

        if p.whole.is_none() && p.raw_row != Some(input_row) {
            match &p.stream {
                Stream::Plain => {
                    let stride = p.raw.len() as u64;
                    // See `begin_pixels`: reaching a non-attachment location here would be
                    // this decoder contradicting its own `is_streamable`, not a file defect.
                    let Location::Attachment { position, .. } = p.plan.location else {
                        unreachable!(
                            "internal invariant violated: a streaming decode was started on a non-attachment location"
                        );
                    };
                    // Checked, both operations. `position` is not a product of the geometry:
                    // it is parsed straight out of the `location="attachment:P:S"` attribute,
                    // so a file states it and nothing upstream bounds it on a source of
                    // unknown length. `input_row * stride` is bounded by the geometry, but
                    // only by the *raised* cap when a caller has raised one — § The caps
                    // contemplates exactly that. Together they reached `u64`: an
                    // `attempt to add with overflow` panic under the checks every test and
                    // fuzz build carries, and worse without them, because a wrap lands on a
                    // small offset and the read then succeeds against the wrong bytes.
                    //
                    // `src/reader.rs`'s `check_total_samples` bounds products of the geometry
                    // and says in its own comment that it does not bound this. This site is
                    // bounded here instead.
                    let offset = input_row
                        .checked_mul(stride)
                        .and_then(|row_at| position.checked_add(row_at))
                        .ok_or_else(|| {
                            Error::limit(format!(
                                "row offset: the block at position {position} plus row \
                                 {input_row} at {stride} bytes a row ends past the 64-bit \
                                 arithmetic every offset here runs in"
                            ))
                        })?;
                    if src.position() != offset {
                        src.seek_to(offset, limits)?;
                    }
                    src.read_exact(&mut p.raw)?;
                }
                _ => {
                    // A compressed stream cannot seek, so reaching a later input row means
                    // reading and discarding the ones before it. Every cursor this decoder
                    // produces is monotone in the input row, so a row is never wanted twice.
                    while p.stream_row <= input_row {
                        let mut raw = std::mem::take(&mut p.raw);
                        let outcome = stream_fill(p, src, limits, &mut raw);
                        p.raw = raw;
                        outcome?;
                        p.stream_row += 1;
                    }
                }
            }
            p.raw_row = Some(input_row);
        }

        decode_row(p, channel, row, width, height, channels)?;

        p.chunks_done += 1;

        // The last chunk of a compressed stream is where a decompression bomb is caught: the
        // stream produced exactly the geometry-implied length and is now required to be at end
        // of stream, per § Hardening. Without it the bomb's first rows decode correctly and the
        // remaining megabytes are simply never pulled, so the file decodes clean.
        //
        // Only a decode that consumed every input row may require it: `select_channel` on a
        // planar block legitimately stops before the block ends, and the rows it never asked
        // for are not evidence of anything.
        if p.chunks_done >= p.chunks_total
            && p.whole.is_none()
            && !matches!(p.stream, Stream::Plain)
        {
            let input_rows = match p.plan.storage {
                PixelStorage::Planar => height * channels,
                PixelStorage::Normal => height,
            };
            if p.stream_row >= input_rows {
                let PixelState {
                    stream,
                    stored_left,
                    stored_at,
                    ..
                } = p;
                stream.require_end(src, stored_left, stored_at, limits)?;
            }
        }

        Ok(Some(ChunkMeta {
            channel: channel as u32,
            start: narrow(dest_start, "chunk destination offset")?,
            len: g.width as usize,
        }))
    }

    pub(crate) fn scratch(&self, len: usize) -> SampleSlice<'_> {
        match self.pixels.as_ref() {
            None => SampleSlice::U8(&[]),
            Some(p) => slice_samples(&p.scratch, len),
        }
    }
}

/// Fill `out` from the compressed stream, crossing subblock boundaries as it goes.
///
/// A row is not aligned to a subblock, so one read may finish one subblock and continue into
/// the next.
fn stream_fill<S: Source>(
    p: &mut PixelState,
    src: &mut S,
    limits: &Limits,
    out: &mut [u8],
) -> Result<()> {
    let mut filled = 0usize;
    while filled < out.len() {
        if p.subblock_uncompressed_left == 0 {
            open_next_subblock(p, src, limits)?;
        }
        let want = narrow(p.subblock_uncompressed_left, "subblock output")?;
        let take = (out.len() - filled).min(want);
        {
            let PixelState {
                stream,
                stored_left,
                stored_at,
                ..
            } = p;
            stream.fill(
                &mut out[filled..filled + take],
                src,
                stored_left,
                stored_at,
                limits,
            )?;
        }
        p.subblock_uncompressed_left -= take as u64;
        filled += take;
    }
    Ok(())
}

/// Finish the current subblock and open the next.
///
/// The finished stream is required to be at end of stream **per subblock**, so a bomb hidden
/// in any one of them is caught at its own boundary rather than at the end of the block.
fn open_next_subblock<S: Source>(p: &mut PixelState, src: &mut S, limits: &Limits) -> Result<()> {
    {
        let PixelState {
            stream,
            stored_left,
            stored_at,
            ..
        } = p;
        stream.require_end(src, stored_left, stored_at, limits)?;
    }

    // Read the next subblock's declared lengths and its start offset under one shared borrow,
    // then drop it before mutating. Cloning the list here instead would allocate once per
    // boundary — with the subblock cap at 4096 that is quadratic, on the one path whose whole
    // discipline is not allocating from a declared size.
    let index = p.subblock_index + 1;
    let (compressed, uncompressed, start) = {
        let list = p.plan.subblocks.as_deref().unwrap_or(&[]);
        let Some(&(compressed, uncompressed)) = list.get(index) else {
            return Err(Error::malformed(
                "compressed block ended before the geometry-implied size was reached",
            ));
        };
        // The boundary comes from the declared compressed lengths, not from how much the codec
        // consumed: a codec may legitimately stop short of the bytes it was given.
        let start = list
            .iter()
            .take(index)
            .try_fold(p.block_start, |at, (c, _)| at.checked_add(*c))
            .ok_or_else(|| Error::malformed("subblock offsets overflow the address space"))?;
        (compressed, uncompressed, start)
    };
    p.subblock_index = index;
    p.stored_at = start;
    p.stored_left = compressed;
    p.subblock_uncompressed_left = uncompressed;

    let codec = p
        .plan
        .compression
        .ok_or_else(|| Error::malformed("subblocks without a compression codec"))?
        .codec;
    let PixelState {
        stream,
        stored_left,
        stored_at,
        ..
    } = p;
    // `Stream::open` owns both halves of the boundary. It reuses the codec state a framed
    // codec carries across the whole block — constructing one per subblock multiplies a fixed
    // cost by `Subblock count`, which is a cap — and it drops the outgoing subblock's buffers
    // before sizing the incoming one's, so the two whole-subblock LZ4 buffers are never
    // resident together. § Streaming promises one subblock, compressed and decompressed, not
    // two.
    stream.open(codec, src, stored_left, stored_at, uncompressed, limits)?;
    Ok(())
}

/// Read a declared length in fixed chunks rather than pre-allocating from it.
fn read_incrementally<S: Source>(src: &mut S, declared: u64) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; HEADER_CHUNK];
    let mut left = declared;
    while left > 0 {
        let want = left.min(HEADER_CHUNK as u64) as usize;
        src.read_exact(&mut chunk[..want])?;
        out.extend_from_slice(&chunk[..want]);
        left -= want as u64;
    }
    Ok(out)
}

/// Checksums are verified for every block whose contents are **actually read**.
///
/// That makes tier 1 free for `attachment` blocks and **not** free for `embedded` ones, whose
/// contents are read during the header parse and whose digest is therefore verified at
/// construction. §10.5 permits on-demand verification, so this is where "on demand" lands for
/// each location mode — and it is why a mismatched embedded digest fails the source outright
/// rather than surfacing as a declined position.
fn verify_materialized_blocks(occurrences: &[Occurrence]) -> Result<()> {
    for occurrence in occurrences {
        let Some(plan) = &occurrence.plan else {
            continue;
        };
        let (Some(checksum), Some(bytes)) = (&plan.checksum, &plan.materialized) else {
            continue;
        };
        codec::verify(checksum, bytes)?;
    }
    Ok(())
}

/// An `attachment` position must lie beyond the header region.
///
/// Nothing else rules this out, and a declared position of, say, 40 on a seekable source
/// would hand the caller the XML header's own bytes as pixel samples: a decode that looks
/// plausible and is fabricated. One comparison closes it. It runs at the header phase, where
/// the validation order puts `location`, so it declines the position rather than failing at
/// the first pixel call.
fn refuse_attachments_inside_the_header(occurrences: &mut [Occurrence], header_end: u64) {
    for occurrence in occurrences.iter_mut() {
        let Some(plan) = &occurrence.plan else {
            continue;
        };
        let Location::Attachment { position, .. } = plan.location else {
            continue;
        };
        if position >= header_end {
            continue;
        }
        occurrence.header.decline_reason = Some(DeclineReason::new(
            DeclineClass::Malformed,
            format!(
                "attachment position {position} lies inside the header region, which ends at \
                 {header_end}"
            ),
        ));
        occurrence.header.granularity = Granularity::WholeImage;
        occurrence.plan = None;
    }
}

/// Whether this block's samples can be pulled a row at a time straight from the source.
///
/// A checksum covers the whole **stored** block (§10.5) and §10.6.1 forbids using any of it
/// before verification, so a checksummed block is never streamable however it is compressed.
fn is_streamable(block: &BlockPlan) -> bool {
    if !matches!(block.location, Location::Attachment { .. }) || block.checksum.is_some() {
        return false;
    }
    match &block.compression {
        None => true,
        // Byte shuffling cannot stream: §10.6.2 spreads one sample's bytes across the entire
        // block, so no prefix yields any complete sample.
        Some(c) if c.shuffled => false,
        // A bare LZ4 block has no framing and decompresses only as a whole -- so it streams
        // only where `subblocks` splits it, one subblock held at a time. That is the `Block`
        // floor `granularity()` reports for exactly this case, and reporting `Block` while
        // materializing the whole image would make the reported floor a fiction.
        Some(c) if matches!(c.codec, Codec::Lz4 | Codec::Lz4Hc) => block.subblocks.is_some(),
        Some(c) => matches!(c.codec, Codec::Zlib | Codec::Zstd),
    }
}

/// The byte range one `decode_row` call touches, checked once rather than once per byte in
/// the loops `decode_row` runs.
///
/// `first_sample`, `stride` and `row_base` mirror the arithmetic `decode_row` computes for the
/// same call — duplicated here rather than threaded back out, so the hot loop is unchanged. A
/// zero-width row touches nothing and needs no range.
fn validate_row_range(
    p: &PixelState,
    first_sample: u64,
    stride: u64,
    width: u64,
    sample_bytes: u64,
    row_base: u64,
) -> Result<()> {
    if width == 0 {
        return Ok(());
    }
    let last_sample = first_sample + (width - 1) * stride;
    let first_byte = first_sample * sample_bytes;
    let last_byte = last_sample * sample_bytes + (sample_bytes - 1);
    let (base, len) = match &p.whole {
        Some(bytes) => (0, bytes.len() as u64),
        None => (row_base, p.raw.len() as u64),
    };
    if first_byte < base || last_byte >= base.saturating_add(len) {
        return Err(Error::malformed(format!(
            "block byte range {first_byte}..={last_byte} falls outside the {len}-byte block at \
             offset {base}"
        )));
    }
    Ok(())
}

/// Fill the scratch buffer with one channel's run of one row.
fn decode_row(
    p: &mut PixelState,
    channel: u64,
    row: u64,
    width: u64,
    height: u64,
    channels: u64,
) -> Result<()> {
    let sample_bytes = u64::from(p.plan.format.bytes());
    let big_endian = p.plan.big_endian;

    // Where this run's samples sit in the block, and how far apart. `Planar` stores each
    // channel's plane whole; `Normal` interleaves, so one channel's row is a strided walk.
    let (first_sample, stride) = match p.plan.storage {
        PixelStorage::Planar => ((channel * height + row) * width, 1u64),
        PixelStorage::Normal => ((row * width) * channels + channel, channels),
    };
    let row_base = match (p.plan.storage, &p.whole) {
        (_, Some(_)) => 0,
        (PixelStorage::Planar, None) => first_sample * sample_bytes,
        (PixelStorage::Normal, None) => row * width * channels * sample_bytes,
    };
    validate_row_range(p, first_sample, stride, width, sample_bytes, row_base)?;

    // The three facts that decide where a byte comes from are constant for the whole row, so
    // they are read **once here** rather than re-tested per byte inside the loop. That is not
    // only fewer branches: while a per-byte helper served all three cases, its shuffled arm's
    // codegen governed the whole function, and giving that arm a bounds-check panic path cost
    // the *unshuffled* case a measured 2x — a branch it never takes. Hoisting the decision
    // puts each case in its own loop, where nothing another case does can reach it.
    let bytes: &[u8] = match &p.whole {
        Some(whole) => whole,
        None => &p.raw,
    };
    // A materialized block is indexed absolutely; a streamed row holds only its own bytes.
    let origin = if p.whole.is_some() { 0 } else { row_base };
    // Narrowed with the checked helper, not `as`. `item-size` is a file-declared `u64` and
    // § The caps requires every such narrowing to be checked "because a caller may raise a cap
    // past what a 32-bit target can address". An `as` truncation would not panic here — a
    // truncated-to-zero item size early-returns as the identity — it would silently compute a
    // *different* permutation and hand back wrong pixels, on the one target (`i686`) this crate
    // runs a lane for precisely to keep its 32-bit claims true.
    let unshuffle = match p
        .plan
        .compression
        .filter(|c| c.shuffled)
        .and_then(|c| c.item_size)
    {
        Some(item) => Some(narrow(item, "shuffle item size")?),
        None => None,
    };

    macro_rules! fill {
        ($v:expr, $w:expr, $be:expr, $le:expr) => {{
            let convert = |b: [u8; $w]| if big_endian { $be(b) } else { $le(b) };
            match unshuffle {
                // The dominant shape by far — every uncompressed, zlib, zstd and planar
                // frame — and the only one where the row is a contiguous run, so it reads as
                // one `chunks_exact` the compiler can vectorize.
                None if stride == 1 => {
                    let start = (first_sample * sample_bytes - origin) as usize;
                    let row = &bytes[start..start + $v.len() * $w];
                    for (dst, b) in $v.iter_mut().zip(row.chunks_exact($w)) {
                        let mut a = [0u8; $w];
                        a.copy_from_slice(b);
                        *dst = convert(a);
                    }
                }
                // Interleaved storage: one channel's row is a strided walk, still direct.
                None => {
                    for (x, dst) in $v.iter_mut().enumerate() {
                        let at =
                            ((first_sample + x as u64 * stride) * sample_bytes - origin) as usize;
                        let mut a = [0u8; $w];
                        a.copy_from_slice(&bytes[at..at + $w]);
                        *dst = convert(a);
                    }
                }
                // §10.6.2 spreads one sample's bytes across the whole block, so this one is
                // irreducibly per byte. It is also always a materialized block, since the
                // shuffle floor forbids streaming.
                Some(item) => {
                    for (x, dst) in $v.iter_mut().enumerate() {
                        let base = (first_sample + x as u64 * stride) * sample_bytes - origin;
                        let mut a = [0u8; $w];
                        for (k, slot) in a.iter_mut().enumerate() {
                            *slot = unshuffled_byte(bytes, item, base as usize + k);
                        }
                        *dst = convert(a);
                    }
                }
            }
        }};
    }

    // Cloning the scratch out and back keeps the borrow checker happy without cloning the
    // buffer: `std::mem::take` leaves an empty Vec behind for the duration.
    let mut scratch = std::mem::replace(&mut p.scratch, Samples::U8(Vec::new()));
    match &mut scratch {
        Samples::U8(v) => match unshuffle {
            None if stride == 1 => {
                let start = (first_sample * sample_bytes - origin) as usize;
                let n = v.len();
                v.copy_from_slice(&bytes[start..start + n]);
            }
            None => {
                for (x, dst) in v.iter_mut().enumerate() {
                    let at = ((first_sample + x as u64 * stride) * sample_bytes - origin) as usize;
                    *dst = bytes[at];
                }
            }
            Some(item) => {
                for (x, dst) in v.iter_mut().enumerate() {
                    let at = ((first_sample + x as u64 * stride) * sample_bytes - origin) as usize;
                    *dst = unshuffled_byte(bytes, item, at);
                }
            }
        },
        Samples::U16(v) => fill!(v, 2, u16::from_be_bytes, u16::from_le_bytes),
        Samples::U32(v) => fill!(v, 4, u32::from_be_bytes, u32::from_le_bytes),
        Samples::U64(v) => fill!(v, 8, u64::from_be_bytes, u64::from_le_bytes),
        Samples::I16(v) => fill!(v, 2, i16::from_be_bytes, i16::from_le_bytes),
        Samples::I32(v) => fill!(v, 4, i32::from_be_bytes, i32::from_le_bytes),
        Samples::I64(v) => fill!(v, 8, i64::from_be_bytes, i64::from_le_bytes),
        Samples::F32(v) => fill!(v, 4, f32::from_be_bytes, f32::from_le_bytes),
        Samples::F64(v) => fill!(v, 8, f64::from_be_bytes, f64::from_le_bytes),
    }
    p.scratch = scratch;
    Ok(())
}

/// Read the stored block, verify it, decompress it, and hand back the still-shuffled result.
///
/// The pipeline order is fixed and nothing may be reordered to save a pass: §10.6.1 is
/// explicit that decompressing a block that failed verification is exploitable.
fn materialize<S: Source>(
    block: &BlockPlan,
    src: &mut S,
    limits: &Limits,
) -> Result<std::sync::Arc<Vec<u8>>> {
    // 1. Read the stored block. Shared rather than copied where it is already resident — an
    //    embedded block's bytes live in the header region this reader already holds — but the
    //    sharing stops here: the pipeline below still runs in full, because a shared buffer is
    //    still a *stored* block and may be compressed, checksummed or both.
    let stored: std::sync::Arc<Vec<u8>> = match &block.location {
        Location::Attachment { position, size } => {
            // Measured against `Materialized bytes`, not merely narrowed. This buffer is the
            // largest thing a `WholeImage` decode allocates and it is sized from a *declared*
            // length, so the cap that § The caps says governs "every buffer this crate
            // allocates for itself" has to see it. Its absence here left the stored block
            // bounded only by the stored-block cap, whose 1 MiB floor applies whatever the
            // geometry — a 1 MB allocation under a 20 000-byte materialization cap — and, on a
            // length-unknown source under the shipped defaults, admitted a stored block and its
            // decompressed form together far above what the cap names. The streaming LZ4 arm
            // already checked its own subblock this way; this is the same rule at the bigger
            // buffer.
            let size = crate::xisf::codec::check_materialized(*size, limits, "a stored block's")?;
            src.seek_to(*position, limits)?;
            let mut buf = vec![0u8; size];
            src.read_exact(&mut buf)?;
            std::sync::Arc::new(buf)
        }
        Location::Embedded | Location::Inline(_) => match &block.materialized {
            Some(bytes) => bytes.clone(),
            None => {
                return Err(Error::malformed(
                    "an embedded block carries no serialized data",
                ));
            }
        },
        Location::External(spelling) => {
            // §10.2 forbids external blocks in a monolithic unit outright, so this is the
            // file contradicting the format rather than a feature this version declines.
            return Err(Error::malformed(format!(
                "block location {spelling}: §10.2 forbids external blocks in a monolithic unit"
            )));
        }
    };

    // 2. Verify, before anything touches the bytes. §10.6.1 is explicit that decompressing a
    //    block that failed verification is exploitable, so nothing may be reordered here to
    //    save a pass. The digest covers the entire *stored* block (§10.5).
    if let Some(checksum) = &block.checksum {
        codec::verify(checksum, &stored)?;
    }

    // 3. Decompress into a destination sized from the geometry, never from a declared length.
    let Some(compression) = &block.compression else {
        if stored.len() as u64 != block.implied_bytes {
            return Err(Error::malformed(format!(
                "uncompressed block holds {} bytes where the geometry implies {}",
                stored.len(),
                block.implied_bytes
            )));
        }
        // Uncompressed: the stored bytes *are* the block, so the buffer is handed on rather
        // than copied.
        return Ok(stored);
    };

    if let Some(list) = &block.subblocks {
        block::check_subblock_sums(list, stored.len() as u64, block.implied_bytes)?;
    }

    let mut out = vec![0u8; narrow(block.implied_bytes, "decompressed block")?];
    codec::decompress(
        &stored,
        compression,
        block.subblocks.as_deref(),
        &mut out,
        limits,
    )?;
    Ok(std::sync::Arc::new(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::Geometry;
    use crate::samples::SampleFormat;

    fn test_plan(
        width: u32,
        channels: u32,
        format: SampleFormat,
        storage: PixelStorage,
    ) -> BlockPlan {
        let geometry = Geometry {
            width,
            height: 1,
            channels,
        };
        let implied_bytes = u64::from(width) * u64::from(channels) * u64::from(format.bytes());
        BlockPlan {
            location: Location::Embedded,
            compression: None,
            subblocks: None,
            checksum: None,
            big_endian: false,
            storage,
            geometry,
            format,
            implied_bytes,
            materialized: None,
        }
    }

    fn test_state(plan: BlockPlan, raw_len: usize) -> PixelState {
        let scratch = Samples::zeroed(plan.format, plan.geometry.width as usize);
        PixelState {
            selected: None,
            chunks_total: 1,
            chunks_done: 0,
            whole: None,
            raw: vec![0u8; raw_len],
            raw_row: None,
            scratch,
            stream: Stream::Plain,
            stream_row: 0,
            stored_left: 0,
            stored_at: 0,
            block_start: 0,
            subblock_index: 0,
            subblock_uncompressed_left: 0,
            plan,
        }
    }

    /// A conforming decode sizes `raw` to exactly one row: width * channels * sample_bytes =
    /// 4 * 1 * 2 = 8 bytes for `U16`. Undersizing it by one byte reproduces a future
    /// off-by-one in the sizing arithmetic, not anything a crafted
    /// file can reach today — and must fail loudly rather than let the row decoder fabricate a
    /// zero pixel for the byte that falls outside `raw`.
    ///
    /// A per-byte helper using `.saturating_sub` / `.unwrap_or(0)` would make this setup return
    /// `Ok` with the last sample silently zeroed instead of reporting the mismatch.
    #[test]
    fn decode_row_errors_on_undersized_raw_buffer_instead_of_fabricating_zero_pixels() {
        let plan = test_plan(4, 1, SampleFormat::U16, PixelStorage::Normal);
        let mut state = test_state(plan, 7);
        let err = decode_row(&mut state, 0, 0, 4, 1, 1).unwrap_err();
        assert!(
            matches!(err, Error::Malformed(_)),
            "unexpected error: {err:?}"
        );
    }

    /// The hoisted check must not reject the buffer size every real decode actually uses.
    #[test]
    fn decode_row_succeeds_with_correctly_sized_raw_buffer() {
        let plan = test_plan(4, 1, SampleFormat::U16, PixelStorage::Normal);
        let mut state = test_state(plan, 8);
        state.raw.copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);
        decode_row(&mut state, 0, 0, 4, 1, 1).expect("a correctly sized row buffer decodes");
        match &state.scratch {
            Samples::U16(v) => assert_eq!(v.len(), 4),
            other => panic!("unexpected scratch variant: {other:?}"),
        }
    }
}
