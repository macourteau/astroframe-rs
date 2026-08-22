//! The FITS decoder: the HDU walk, the decline table, and big-endian sample decode.
//!
//! Structure, card layout, the data-unit size formula and the value grammar are in FITS
//! Standard 4.0 and are not restated here. What is here is what the standard leaves open,
//! plus the hazards whose cost of getting wrong is silent.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::fits::cards::{BLOCK, block_has_end, fold_cards, lex_integer, lex_logical, lex_number};
use crate::header::{
    Bounds, BoundsUnavailable, DeclineClass, DeclineReason, Geometry, Granularity, Header,
    PixelStorage, RowOrder,
};
use crate::limits::{Limits, narrow};
use crate::metadata::{Keyword, KeywordOrigin, KeywordSet, PropertySet};
use crate::normalize::Scaling;
use crate::reader::{ChunkMeta, PixelPlan};
use crate::samples::{SampleFormat, SampleSlice, Samples, slice_samples};
use crate::source::Source;

/// One HDU's parsed header, before any decision about whether to sit on it.
#[derive(Debug)]
struct Hdu {
    /// Shared rather than owned, so that handing this HDU's cards to the `Header` it builds
    /// costs a refcount. See [`KeywordSet`] for what the copy cost when it was one.
    keywords: Arc<[Keyword]>,
    /// Where this HDU's data unit starts.
    data_start: u64,
    /// `XTENSION`'s value, trimmed; `None` for the primary HDU.
    xtension: Option<String>,
}

/// The structural facts a walk needs, each carrying whether it read at all.
#[derive(Clone, Copy, Debug, PartialEq)]
enum UnitSize {
    /// `|BITPIX|/8 × GCOUNT × (PCOUNT + Π NAXISᵢ)`, rounded up to the block boundary.
    Bytes(u64),
    /// A missing, unparseable or out-of-standard-set sizing keyword, or arithmetic that
    /// overflowed. There is no resumption point, so the walk ends.
    Unsizable(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Position {
    /// Nothing selected yet.
    Start,
    /// Sitting on an image position, decodable or declined.
    OnImage,
    /// The walk ended, normally or otherwise.
    Done,
}

/// What the reader needs to deliver pixels from the position it sits on.
#[derive(Debug)]
struct PixelState {
    data_start: u64,
    width: u32,
    height: u32,
    selected: Option<u32>,
    /// The next file row to deliver, counted over `height * channels` rows.
    next_file_row: u64,
    /// How many rows this decode delivers in total.
    rows_to_deliver: u64,
    raw: Vec<u8>,
    scratch: Samples,
}

/// A FITS source, walked forward one image at a time.
#[derive(Debug)]
pub(crate) struct Decoder {
    /// The primary header's cards, re-tagged `PrimaryHeader` — the list every image
    /// extension's report ends with, held once for the whole source.
    primary_keywords: Arc<[Keyword]>,
    /// The primary header's own structural facts, needed for `INHERIT`.
    primary_own: Vec<Keyword>,
    /// The primary's `ROWORDER`, classified **once for the source**.
    ///
    /// `INHERIT = T` applies it to every extension, and a `ROWORDER` value is a keyword value
    /// — bounded by `Assembled keyword value`, not by one card's eighty bytes, a `CONTINUE`
    /// chain assembling it. Classifying it per image position built one copy per extension:
    /// 189 MB allocated and 64 MB held live from a 1.06 MB input. Held classified rather than
    /// as text, so the applied value is a clone of an `Arc` at each position.
    primary_row_order: RowOrder,
    position: Position,
    /// The HDU whose header is parsed and whose data unit has not been consumed.
    pending: Option<Hdu>,
    /// The HDU the reader is sitting on.
    current: Option<Hdu>,
    header: Option<Header>,
    pixels: Option<PixelState>,
    /// Set once the current image's samples have been read, which is what makes a
    /// re-read need a seek. Distinct from the cursor having reached the *padded* end of the
    /// data unit, which is what the walk needs and which FITS rounds up to 2880 bytes.
    pixels_read: bool,
}

impl Decoder {
    /// Parse the primary header and stop. No image is selected: construction reads only
    /// enough to identify the format and parse the first header unit.
    pub(crate) fn new<S: Source>(
        signature: &[u8; 8],
        src: &mut S,
        limits: &Limits,
    ) -> Result<Decoder> {
        let region = read_header_region(signature, src, limits)?;
        let keywords = fold_cards(
            &region,
            KeywordOrigin::Image,
            limits.fits_header_cards,
            limits.keyword_value_bytes,
        )?;

        // Validation order, step 2: `SIMPLE` before anything structural.
        match keyword_value(&keywords, "SIMPLE").and_then(lex_logical) {
            Some(true) => {}
            Some(false) => {
                return Err(Error::unsupported(
                    "SIMPLE = F: this file does not conform to the FITS standard, and this \
                     version reads only conforming files",
                ));
            }
            None => {
                return Err(Error::malformed(
                    "the primary header's SIMPLE card is missing or is not a logical value",
                ));
            }
        }

        let data_start = src.position();
        Ok(Decoder {
            primary_keywords: reorigin(&keywords, KeywordOrigin::PrimaryHeader).into(),
            primary_row_order: keyword_value(&keywords, "ROWORDER")
                .map(RowOrder::classify)
                .unwrap_or(RowOrder::Unspecified),
            primary_own: keywords.clone(),
            position: Position::Start,
            pending: Some(Hdu {
                keywords: keywords.into(),
                data_start,
                xtension: None,
            }),
            current: None,
            header: None,
            pixels: None,
            pixels_read: false,
        })
    }

    pub(crate) fn header(&self) -> Option<&Header> {
        self.header.as_ref()
    }

    /// FITS declares no `bounds`, so there is never any declared text to preserve.
    pub(crate) fn declared_bounds_text(&self) -> Option<String> {
        None
    }

    /// Advance to the next image position.
    ///
    /// Non-image extensions are **skipped**, not errors: a file may legitimately interleave
    /// tables between images, and refusing the source over one would reject ordinary MEF
    /// files. An `IMAGE` extension declaring `NAXIS = 0` is skipped with them, since it holds
    /// no image either — the same header content must not classify differently for sitting in
    /// an extension rather than in the primary.
    pub(crate) fn next_image<S: Source>(&mut self, src: &mut S, limits: &Limits) -> Result<bool> {
        if self.position == Position::Done {
            return Ok(false);
        }

        if self.position == Position::OnImage {
            // Step over the image we were sitting on. An HDU whose data unit cannot be sized
            // has no resumption point — inventing one misparses every HDU after it — so the
            // walk ends here rather than guessing.
            if let Err(e) = self.consume_current(src, limits) {
                self.position = Position::Done;
                return Err(e);
            }
            match self.read_next_hdu(src, limits) {
                Ok(next) => self.pending = next,
                Err(e) => {
                    self.position = Position::Done;
                    return Err(e);
                }
            }
        }

        let mut traversed: u32 = 0;
        loop {
            let Some(hdu) = self.pending.take() else {
                self.position = Position::Done;
                self.header = None;
                return Ok(false);
            };

            if is_image_position(&hdu) {
                let header = build_header(
                    &hdu,
                    &self.primary_keywords,
                    &self.primary_own,
                    &self.primary_row_order,
                );
                self.header = Some(header);
                self.current = Some(hdu);
                self.position = Position::OnImage;
                self.pixels = None;
                self.pixels_read = false;
                return Ok(true);
            }

            // A position the walk steps over rather than sits on. Both kinds count against
            // the traversal cap: over a pipe there is no file length to terminate the loop,
            // and a hang is what invariant I5 names alongside panics.
            traversed = traversed.saturating_add(1);
            if traversed > limits.hdus_per_advance {
                self.position = Position::Done;
                return Err(Error::limit(format!(
                    "HDUs traversed per advance: more than {} extensions were stepped over \
                     inside one next_image()",
                    limits.hdus_per_advance
                )));
            }

            let outcome = self
                .skip_data_unit(&hdu, src, limits)
                .and_then(|()| self.read_next_hdu(src, limits));
            match outcome {
                Ok(next) => self.pending = next,
                Err(e) => {
                    self.position = Position::Done;
                    return Err(e);
                }
            }
        }
    }

    /// Skip whatever remains of the current image's data.
    pub(crate) fn abandon<S: Source>(&mut self, src: &mut S, limits: &Limits) -> Result<()> {
        self.consume_current(src, limits)
    }

    /// Move the cursor to the **padded** end of the current HDU's data unit.
    ///
    /// Always computed rather than tracked: the pixel phase reads only the sample bytes, and
    /// FITS pads a data unit up to the 2880-byte block boundary. Resuming the walk from the
    /// unpadded end misparses every HDU after it.
    fn consume_current<S: Source>(&mut self, src: &mut S, limits: &Limits) -> Result<()> {
        let Some(hdu) = self.current.as_ref() else {
            return Ok(());
        };
        let end = match data_unit_size(&hdu.keywords, hdu.xtension.is_none()) {
            // `checked_add`, not `+`. `data_unit_size` checks its own arithmetic and can still
            // hand back a legitimate `18446744073709551360` — the largest multiple of 2880
            // below `u64::MAX` — from `NAXIS1 = 9223372036854774368` at `BITPIX = 16`, inside
            // one 2880-byte primary header with no cap raised. Adding the file offset to that
            // overflows, which is a panic under the overflow checks the test and fuzz profiles
            // use and a silent wrap without them. § Errors names this outcome: size arithmetic
            // that overflows the u64 the computation runs in is `Malformed` from the *next*
            // `next_image()`, which is where this sits. `skip_data_unit` never had the defect
            // because it skips the two spans separately rather than summing them.
            UnitSize::Bytes(n) => match hdu.data_start.checked_add(n) {
                Some(end) => end,
                None => {
                    return Err(Error::malformed(format!(
                        "cannot size the data unit at offset {}: its {n}-byte extent ends past u64; \
                         there is no resumption point, so the walk ends here",
                        hdu.data_start
                    )));
                }
            },
            UnitSize::Unsizable(why) => {
                return Err(Error::malformed(format!(
                    "cannot size the data unit at offset {}: {why}; there is no resumption \
                     point, so the walk ends here",
                    hdu.data_start
                )));
            }
        };
        if src.position() < end {
            src.skip(end - src.position(), limits)?;
        }
        self.pixels = None;
        Ok(())
    }

    fn skip_data_unit<S: Source>(&self, hdu: &Hdu, src: &mut S, limits: &Limits) -> Result<()> {
        match data_unit_size(&hdu.keywords, hdu.xtension.is_none()) {
            UnitSize::Bytes(n) => {
                if src.position() < hdu.data_start {
                    src.skip(hdu.data_start - src.position(), limits)?;
                }
                if n > 0 {
                    src.skip(n, limits)?;
                }
                Ok(())
            }
            UnitSize::Unsizable(why) => Err(Error::malformed(format!(
                "cannot size the data unit of the extension at offset {}: {why}",
                hdu.data_start
            ))),
        }
    }

    /// Read one more header unit, or `None` at end of source.
    fn read_next_hdu<S: Source>(&self, src: &mut S, limits: &Limits) -> Result<Option<Hdu>> {
        let mut first = [0u8; BLOCK];
        let mut filled = 0usize;
        while filled < BLOCK {
            let got = src.read_some(&mut first[filled..])?;
            if got == 0 {
                break;
            }
            filled += got;
        }
        if filled == 0 {
            return Ok(None);
        }
        if filled < BLOCK {
            return Err(Error::malformed(format!(
                "truncated header block: {filled} of {BLOCK} bytes before the source ended"
            )));
        }

        let region = read_header_region_from(first, src, limits)?;
        let keywords = fold_cards(
            &region,
            KeywordOrigin::Image,
            limits.fits_header_cards,
            limits.keyword_value_bytes,
        )?;
        let xtension = keyword_value(&keywords, "XTENSION")
            .map(|v| v.trim().to_owned())
            .ok_or_else(|| {
                Error::malformed(format!(
                    "the header unit ending at offset {} carries no XTENSION card",
                    src.position()
                ))
            })?;
        Ok(Some(Hdu {
            keywords: keywords.into(),
            data_start: src.position(),
            xtension: Some(xtension),
        }))
    }

    // ------------------------------------------------------------- pixel phase

    pub(crate) fn begin_pixels<S: Source>(
        &mut self,
        src: &mut S,
        limits: &Limits,
        plan: &PixelPlan,
    ) -> Result<()> {
        let hdu = self
            .current
            .as_ref()
            .ok_or_else(|| Error::invalid_request("no image is selected"))?;
        let header = self
            .header
            .as_ref()
            .ok_or_else(|| Error::invalid_request("no image is selected"))?;

        if self.pixels_read {
            if !src.is_seekable() {
                return Err(Error::unsupported(
                    "re-reading an image's pixels needs a seekable source; this one is \
                     forward-only",
                ));
            }
            self.pixels_read = false;
        }

        let g = header
            .geometry
            .ok_or_else(|| Error::invalid_request("this position reports no geometry"))?;
        let format = header
            .sample_format
            .ok_or_else(|| Error::invalid_request("this position reports no sample format"))?;

        let sample_bytes = u64::from(format.bytes());
        let stored = g
            .total_samples()
            .checked_mul(sample_bytes)
            .ok_or_else(|| Error::malformed("geometry times sample width overflows u64"))?;

        // On a source of known length, geometry gets a second and much tighter bound: an
        // uncompressed image cannot need more stored bytes than the file contains. It runs
        // here rather than during header decode, and that placement is load-bearing — a
        // size-capped *prefix* handed to `Reader::seekable` has exactly this shape, and
        // refusing it would break header-only decode.
        if let Some(len) = src.length() {
            let need = hdu.data_start.saturating_add(stored);
            if need > len {
                return Err(Error::malformed(format!(
                    "geometry {}x{}x{} at {} bytes per sample needs {stored} bytes from offset \
                     {}, but the source is {len} bytes long",
                    g.width, g.height, g.channels, sample_bytes, hdu.data_start
                )));
            }
        }

        // The row-shaped staging buffer is one of the buffers `Materialized bytes` governs,
        // measured at its own size before it is sized. Nothing forbids a huge image being a
        // single row.
        let row_bytes = u64::from(g.width) * sample_bytes;
        if row_bytes > limits.materialized_bytes {
            return Err(Error::limit(format!(
                "materialized bytes: one row of {} samples at {sample_bytes} bytes is \
                 {row_bytes} bytes, above the {} byte cap",
                g.width, limits.materialized_bytes
            )));
        }
        let row_bytes = narrow(row_bytes, "row staging buffer")?;

        src.seek_to(hdu.data_start, limits)?;

        let rows_to_deliver = match plan.selected_channel {
            Some(_) => u64::from(g.height),
            None => u64::from(g.height) * u64::from(g.channels),
        };

        self.pixels = Some(PixelState {
            data_start: hdu.data_start,
            width: g.width,
            height: g.height,
            selected: plan.selected_channel,
            next_file_row: 0,
            rows_to_deliver,
            raw: vec![0u8; row_bytes],
            scratch: Samples::zeroed(format, g.width as usize),
        });
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
        if p.next_file_row >= p.rows_to_deliver {
            // Stepping over the rest of the data unit is `consume_current`'s job: it knows
            // the *padded* end, which is what the walk resumes from.
            self.pixels_read = true;
            return Ok(None);
        }

        // Which file row this is, and where its samples land in the destination. With
        // `Planar` storage the unwanted channels are contiguous runs, so a narrowed decode
        // skips them outright rather than reading and discarding samples one by one.
        let (file_row, dest_start) = match p.selected {
            Some(k) => {
                let y = p.next_file_row;
                (
                    u64::from(k) * u64::from(p.height) + y,
                    y * u64::from(p.width),
                )
            }
            None => (p.next_file_row, p.next_file_row * u64::from(p.width)),
        };

        // Checked, though the byte-extent gate in `Reader::check_total_samples` already makes
        // this unreachable: `file_row * row_bytes` is strictly less than the frame's byte
        // extent, and a geometry whose extent does not fit `u64` never gets here. Written
        // checked anyway, because that argument lives in another module and a later change to
        // the gate would re-open this silently. The XISF twin of this line was a bare `+` on a
        // *declared* offset and did overflow — see `src/xisf/decoder.rs`.
        let row_offset = (p.raw.len() as u64)
            .checked_mul(file_row)
            .and_then(|at| p.data_start.checked_add(at))
            .ok_or_else(|| {
                Error::limit(format!(
                    "row offset: the data unit at {} plus row {file_row} at {} bytes a row \
                     ends past the 64-bit arithmetic every offset here runs in",
                    p.data_start,
                    p.raw.len()
                ))
            })?;
        if src.position() != row_offset {
            src.seek_to(row_offset, limits)?;
        }
        src.read_exact(&mut p.raw)?;
        decode_be(&p.raw, &mut p.scratch);

        p.next_file_row += 1;
        let channel = match p.selected {
            Some(k) => k,
            None => (file_row / u64::from(p.height)) as u32,
        };
        Ok(Some(ChunkMeta {
            channel,
            start: narrow(dest_start, "chunk destination offset")?,
            len: p.width as usize,
        }))
    }

    pub(crate) fn scratch(&self, len: usize) -> SampleSlice<'_> {
        let empty = SampleSlice::U8(&[]);
        let Some(p) = self.pixels.as_ref() else {
            return empty;
        };
        slice_samples(&p.scratch, len)
    }
}

// ------------------------------------------------------------------ header reading

fn read_header_region<S: Source>(
    signature: &[u8; 8],
    src: &mut S,
    limits: &Limits,
) -> Result<Vec<u8>> {
    let mut first = [0u8; BLOCK];
    first[..8].copy_from_slice(signature);
    src.read_exact(&mut first[8..])?;
    read_header_region_from(first, src, limits)
}

/// Read 2880-byte blocks until one contains `END`, bounded by the header-byte cap.
///
/// FITS has no declared header length to bound this, and over a pipe there is no file length
/// either, so without the cap the keyword list grows until the process dies.
fn read_header_region_from<S: Source>(
    first: [u8; BLOCK],
    src: &mut S,
    limits: &Limits,
) -> Result<Vec<u8>> {
    let mut region = Vec::from(first);
    while !block_has_end(&region[region.len() - BLOCK..]) {
        if region.len() as u64 + BLOCK as u64 > limits.fits_header_bytes {
            return Err(Error::limit(format!(
                "FITS header length: no END card within {} bytes",
                limits.fits_header_bytes
            )));
        }
        let mut next = [0u8; BLOCK];
        src.read_exact(&mut next)?;
        region.extend_from_slice(&next);
    }
    Ok(region)
}

fn reorigin(keywords: &[Keyword], origin: KeywordOrigin) -> Vec<Keyword> {
    keywords
        .iter()
        .cloned()
        .map(|mut k| {
            k.origin = origin;
            k
        })
        .collect()
}

fn keyword_value<'a>(keywords: &'a [Keyword], name: &str) -> Option<&'a str> {
    keywords
        .iter()
        .find(|k| k.name() == name)
        .map(|k| k.value())
}

// ------------------------------------------------------------------ the decline table

/// Whether the reader sits on this HDU or steps over it.
///
/// A primary with `NAXIS = 0` is the ordinary shape of every multi-extension file, so it is
/// not a decline at all — only the *absence of any image anywhere* is left, and that is end
/// of source rather than an error. An `XTENSION = 'IMAGE'` with `NAXIS = 0` answers the same
/// question the same way. A `BINTABLE` carrying `ZIMAGE = T` is the opposite: a declined
/// position rather than an aborted source, so the second-pass dispatch point has somewhere to
/// attach.
fn is_image_position(hdu: &Hdu) -> bool {
    let naxis = keyword_value(&hdu.keywords, "NAXIS").and_then(lex_integer);
    match hdu.xtension.as_deref() {
        None | Some("IMAGE") => match naxis {
            // A missing or unparseable NAXIS is a declined image position rather than an
            // unsizable skip: the reader sits on it and reports what it can.
            None => true,
            Some(0) => false,
            Some(_) => true,
        },
        Some("BINTABLE") => {
            keyword_value(&hdu.keywords, "ZIMAGE").and_then(lex_logical) == Some(true)
        }
        _ => false,
    }
}

/// Read the geometry, if it reads at all.
///
/// The line is **representability, not validity**: a geometry this crate can read is reported
/// even when what it reads is what declines the position. So this is computed independently
/// of which fault wins the class.
fn read_geometry(keywords: &[Keyword], prefix: &str) -> Option<Geometry> {
    let naxis = keyword_value(keywords, &format!("{prefix}NAXIS")).and_then(lex_integer)?;
    let axis = |i: i64| -> Option<u32> {
        let v = keyword_value(keywords, &format!("{prefix}NAXIS{i}")).and_then(lex_integer)?;
        u32::try_from(v).ok()
    };
    match naxis {
        2 => Some(Geometry {
            width: axis(1)?,
            height: axis(2)?,
            channels: 1,
        }),
        3 => Some(Geometry {
            width: axis(1)?,
            height: axis(2)?,
            channels: axis(3)?,
        }),
        _ => None,
    }
}

fn read_sample_format(keywords: &[Keyword], prefix: &str) -> Option<SampleFormat> {
    let bitpix = keyword_value(keywords, &format!("{prefix}BITPIX")).and_then(lex_integer)?;
    sample_format_of(bitpix)
}

fn sample_format_of(bitpix: i64) -> Option<SampleFormat> {
    match bitpix {
        8 => Some(SampleFormat::U8),
        16 => Some(SampleFormat::I16),
        32 => Some(SampleFormat::I32),
        64 => Some(SampleFormat::I64),
        -32 => Some(SampleFormat::F32),
        -64 => Some(SampleFormat::F64),
        _ => None,
    }
}

/// Walk the FITS validation order and return the first fault, if any.
///
/// > block and card structure → `SIMPLE`/`XTENSION` → `BITPIX` → `NAXIS` and `NAXISn` →
/// > `PCOUNT` and `GCOUNT` → `GROUPS` → `ZIMAGE` → `BSCALE`/`BZERO`/`BLANK`
///
/// A header can carry two faults of different classes, and the decline table would otherwise
/// not determine which one the caller sees. So a header carrying both `BITPIX = 5` and
/// `NAXIS = 1` is `Malformed`, not `Unsupported`: structural validity of a value is settled
/// before scope is.
fn first_fault(hdu: &Hdu) -> Option<DeclineReason> {
    let kw = &hdu.keywords;
    let is_primary = hdu.xtension.is_none();

    // BITPIX
    match keyword_value(kw, "BITPIX").and_then(lex_integer) {
        None => {
            return Some(DeclineReason::new(
                DeclineClass::Malformed,
                "BITPIX is missing or is not an integer value",
            ));
        }
        Some(b) if sample_format_of(b).is_none() => {
            return Some(DeclineReason::new(
                DeclineClass::Malformed,
                format!("BITPIX = {b} is outside the standard set {{8, 16, 32, 64, -32, -64}}"),
            ));
        }
        Some(_) => {}
    }

    // NAXIS and NAXISn
    let naxis = match keyword_value(kw, "NAXIS").and_then(lex_integer) {
        None => {
            return Some(DeclineReason::new(
                DeclineClass::Malformed,
                "NAXIS is missing or is not an integer value",
            ));
        }
        Some(n) if n < 0 => {
            return Some(DeclineReason::new(
                DeclineClass::Malformed,
                format!("NAXIS = {n} is negative"),
            ));
        }
        Some(n) => n,
    };
    for i in 1..=naxis {
        match keyword_value(kw, &format!("NAXIS{i}")).and_then(lex_integer) {
            None => {
                return Some(DeclineReason::new(
                    DeclineClass::Malformed,
                    format!("NAXIS{i} is missing or is not an integer value"),
                ));
            }
            Some(v) if v < 0 => {
                return Some(DeclineReason::new(
                    DeclineClass::Malformed,
                    format!("NAXIS{i} = {v} is negative"),
                ));
            }
            Some(_) => {}
        }
    }

    // PCOUNT and GCOUNT — mandatory in every extension header (§3.4.1); absent and defaulted
    // in the primary.
    if !is_primary {
        for name in ["PCOUNT", "GCOUNT"] {
            if keyword_value(kw, name).and_then(lex_integer).is_none() {
                return Some(DeclineReason::new(
                    DeclineClass::Malformed,
                    format!("{name} is missing or is not an integer value in an extension header"),
                ));
            }
        }
    }

    // GROUPS
    if keyword_value(kw, "GROUPS").and_then(lex_logical) == Some(true) {
        return Some(DeclineReason::new(
            DeclineClass::Unsupported,
            "GROUPS = T: the random-groups structure is not an image this version reads",
        ));
    }

    // ZIMAGE. It sits here rather than first because a header can carry two faults of
    // different classes and the order is what makes the outcome determinate: a tile-compressed
    // BINTABLE whose BITPIX is out of the standard set is `Malformed` on the BITPIX, not
    // `Unsupported` on the tile compression. Reached only when ZIMAGE = T, per
    // `is_image_position`.
    if hdu.xtension.as_deref() == Some("BINTABLE") {
        return Some(DeclineReason::new(
            DeclineClass::Unsupported,
            "tile-compressed image (ZIMAGE = T on a BINTABLE extension): this version declines \
             it rather than misreading it as a table",
        ));
    }

    // Scope, after structural validity is settled.
    if naxis == 1 || naxis > 3 {
        return Some(DeclineReason::new(
            DeclineClass::Unsupported,
            format!("NAXIS = {naxis}: this version reads NAXIS 2, or 3 read as channels"),
        ));
    }
    for i in 1..=naxis {
        match keyword_value(kw, &format!("NAXIS{i}")).and_then(lex_integer) {
            Some(0) => {
                return Some(DeclineReason::new(
                    DeclineClass::Unsupported,
                    format!(
                        "NAXIS{i} = 0: a degenerate axis declares an image with no samples, \
                         which this version declines"
                    ),
                ));
            }
            Some(v) if u32::try_from(v).is_err() => {
                return Some(DeclineReason::new(
                    DeclineClass::Unsupported,
                    format!("NAXIS{i} = {v} is beyond the axis length this version represents"),
                ));
            }
            _ => {}
        }
    }

    // BSCALE / BZERO / BLANK: unparseable values are malformed, but their *scope* — whether
    // the pairing is the unsigned convention — is a bounds question rather than a decline.
    for name in ["BSCALE", "BZERO"] {
        if let Some(v) = keyword_value(kw, name)
            && lex_number(v).is_none()
        {
            return Some(DeclineReason::new(
                DeclineClass::Malformed,
                format!("{name} = {v:?} is not a numeric value"),
            ));
        }
    }
    if let Some(v) = keyword_value(kw, "BLANK")
        && lex_integer(v).is_none()
    {
        return Some(DeclineReason::new(
            DeclineClass::Malformed,
            format!("BLANK = {v:?} is not an integer value"),
        ));
    }

    None
}

/// `|BITPIX|/8 × GCOUNT × (PCOUNT + Π NAXISᵢ)`, rounded up to the 2880-byte block boundary.
///
/// Named in the design because the naive `BITPIX × NAXIS*` form lands mid-file on any
/// heap-carrying `BINTABLE` and misparses everything after it: `PCOUNT` carries a table's heap
/// size. This is also the prerequisite for recognizing a tile-compressed file at all.
///
/// The primary HDU takes `PCOUNT = 0` and `GCOUNT = 1` (§4.4.1.1) with one exception: under
/// `GROUPS = T` both are mandatory and the axis product runs over `NAXIS2`…`NAXISn`, because
/// §6.1.1 fixes `NAXIS1 = 0` and a product including it is zero. The random-groups position is
/// declined, but the walk still has to step over its data unit to reach whatever follows, and
/// a zero-sized step lands inside the group data rather than on the next header.
fn data_unit_size(keywords: &[Keyword], is_primary: bool) -> UnitSize {
    let Some(bitpix) = keyword_value(keywords, "BITPIX").and_then(lex_integer) else {
        return UnitSize::Unsizable("BITPIX is missing or is not an integer value");
    };
    if sample_format_of(bitpix).is_none() {
        return UnitSize::Unsizable("BITPIX is outside the standard set");
    }
    let Some(naxis) = keyword_value(keywords, "NAXIS").and_then(lex_integer) else {
        return UnitSize::Unsizable("NAXIS is missing or is not an integer value");
    };
    if !(0..=999).contains(&naxis) {
        return UnitSize::Unsizable("NAXIS is outside the range the standard allows");
    }
    if naxis == 0 {
        // §7.1.1: no data blocks follow, with PCOUNT zero and GCOUNT one. The skip is exact
        // rather than estimated, so it costs no `Skipped block bytes` at all.
        return UnitSize::Bytes(0);
    }

    let random_groups =
        is_primary && keyword_value(keywords, "GROUPS").and_then(lex_logical) == Some(true);
    let first_axis = if random_groups { 2 } else { 1 };

    // The product of no axes is one everywhere except here: a random-groups header with
    // `NAXIS = 1` has no group data array at all, so each group is its parameters and nothing
    // else, and the term §6.1.1 adds `PCOUNT` to is zero rather than one.
    let mut elements: u64 = u64::from(!(random_groups && naxis < 2));
    for i in first_axis..=naxis {
        let Some(v) = keyword_value(keywords, &format!("NAXIS{i}")).and_then(lex_integer) else {
            return UnitSize::Unsizable("a NAXISn is missing or is not an integer value");
        };
        let Ok(v) = u64::try_from(v) else {
            return UnitSize::Unsizable("a NAXISn is negative");
        };
        let Some(next) = elements.checked_mul(v) else {
            return UnitSize::Unsizable("the axis product overflows u64");
        };
        elements = next;
    }

    let (pcount, gcount) = if is_primary && !random_groups {
        (0u64, 1u64)
    } else {
        let Some(p) = keyword_value(keywords, "PCOUNT").and_then(lex_integer) else {
            return UnitSize::Unsizable("PCOUNT is missing or is not an integer value");
        };
        let Some(g) = keyword_value(keywords, "GCOUNT").and_then(lex_integer) else {
            return UnitSize::Unsizable("GCOUNT is missing or is not an integer value");
        };
        match (u64::try_from(p), u64::try_from(g)) {
            (Ok(p), Ok(g)) => (p, g),
            _ => return UnitSize::Unsizable("PCOUNT or GCOUNT is negative"),
        }
    };

    let width = bitpix.unsigned_abs() / 8;
    let bytes = elements
        .checked_add(pcount)
        .and_then(|n| n.checked_mul(gcount))
        .and_then(|n| n.checked_mul(width));
    let Some(bytes) = bytes else {
        return UnitSize::Unsizable("the data-unit size arithmetic overflows u64");
    };
    let padded = bytes
        .checked_add(BLOCK as u64 - 1)
        .map(|n| n / BLOCK as u64 * BLOCK as u64);
    match padded {
        Some(n) => UnitSize::Bytes(n),
        None => UnitSize::Unsizable("padding the data-unit size to a block boundary overflows u64"),
    }
}

// ------------------------------------------------------------------ header assembly

/// The two cards the `applied` closure below resolves: `BSCALE` and `BZERO`, both lexed to
/// numbers, so re-reading them per image position costs nothing.
///
/// They are two of the four cards `INHERIT` gates the *application* of — the ones that change
/// what a pixel means, which is why they are inheritable at all: applying a primary's `BSCALE`
/// over an extension's own would rewrite every pixel and move the frame between "unsigned
/// convention" and "no normalized output", the silent plausible repair this design refuses
/// everywhere else. The other two are not in this list. `ROWORDER` is resolved beside `applied`
/// rather than through it, because it is the one whose *text* is reported: re-classifying an
/// inherited `ROWORDER` per image position built a copy of an assembled keyword value per
/// extension, so `Decoder::primary_row_order` classifies it once for the source instead — the
/// rule the two spellings implement is the same one stated in `applied`. `BLANK` is resolved
/// nowhere: § FITS decisions reports it and substitutes no sample, so nothing ever asks whether
/// one is inherited.
const INHERITABLE: [&str; 2] = ["BSCALE", "BZERO"];

fn build_header(
    hdu: &Hdu,
    primary_reported: &Arc<[Keyword]>,
    primary_own: &[Keyword],
    primary_row_order: &RowOrder,
) -> Header {
    let is_primary = hdu.xtension.is_none();
    let tile_compressed = hdu.xtension.as_deref() == Some("BINTABLE");
    let prefix = if tile_compressed { "Z" } else { "" };

    // Both headers' cards are always reported when the reader advances to an image
    // extension — the extension's followed by the primary's, each tagged by origin.
    // Reporting is never gated on INHERIT: real archive frames put DATE-OBS, EXPTIME and
    // EGAIN in the primary and frequently omit the keyword.
    // Both lists are shared, never concatenated: `FITS header cards` times `Images per
    // source` is a product no part of the input relates to, and building the merge cost a
    // primary carrying 4090 `HISTORY` cards 52 MB held live across 256 zero-width extensions.
    // `KeywordSet` carries the arithmetic and `Header::keywords` serves the concatenation as a
    // view, exactly as `PropertySet` does for the XISF property merge.
    let keywords = KeywordSet::new(
        hdu.keywords.clone(),
        if is_primary {
            // A primary header inherits from nothing, so there is no second piece.
            Arc::default()
        } else {
            primary_reported.clone()
        },
    );

    let applied = |name: &str| -> Option<&str> {
        if let Some(v) = keyword_value(&hdu.keywords, name) {
            return Some(v);
        }
        // Inheritance fills gaps and never overrides, and the test is **per card**: an
        // extension carrying BSCALE but no BZERO applies its own BSCALE beside the primary's
        // BZERO. Under INHERIT = F, and equally when the extension carries no INHERIT card at
        // all, no primary card is applied. An INHERIT card in a *primary* header gates
        // nothing (Appendix K forbids it there), so it is data rather than an instruction.
        if is_primary || !INHERITABLE.contains(&name) {
            return None;
        }
        if keyword_value(&hdu.keywords, "INHERIT").and_then(lex_logical) != Some(true) {
            return None;
        }
        keyword_value(primary_own, name)
    };

    let geometry = read_geometry(&hdu.keywords, prefix);
    let sample_format = read_sample_format(&hdu.keywords, prefix);
    let decline_reason = first_fault(hdu);

    let bscale = applied("BSCALE").and_then(lex_number).unwrap_or(1.0);
    let bzero = applied("BZERO").and_then(lex_number).unwrap_or(0.0);
    // `applied`'s rule, spelled out rather than called, because `ROWORDER` is the one
    // inheritable card whose *text* is reported: the other three are lexed to numbers. The
    // inherited value is classified once for the whole source and cloned here — see
    // `Decoder::primary_row_order` for what building it per position cost.
    let row_order = if is_primary {
        primary_row_order.clone()
    } else if let Some(text) = keyword_value(&hdu.keywords, "ROWORDER") {
        // The extension's own card, which is that extension's own input bytes.
        RowOrder::classify(text)
    } else if keyword_value(&hdu.keywords, "INHERIT").and_then(lex_logical) == Some(true) {
        primary_row_order.clone()
    } else {
        RowOrder::Unspecified
    };

    let bounds = fits_bounds(sample_format, bscale, bzero, decline_reason.is_some());

    Header {
        geometry,
        sample_format,
        bounds,
        scaling: Some(Scaling::Fits { bscale, bzero }),
        row_order: Some(row_order),
        orientation: None,
        offset: None,
        color_space: None,
        pixel_storage: Some(PixelStorage::Planar),
        image_id: None,
        image_uuid: None,
        image_type: None,
        channel_index: None,
        granularity: if decline_reason.is_some() {
            Granularity::WholeImage
        } else {
            Granularity::Rows
        },
        decline_reason,
        keywords,
        properties: PropertySet::default(),
        cfa: None,
        // FITS defines neither concept, so there is nothing to report and no default that
        // would belong to this format: § The API's rule is `None` rather than a fabricated
        // value, the same answer `orientation()` gives here.
        resolution: None,
        display_function: None,
    }
}

/// Where the representable range comes from for a FITS image.
///
/// Normalized output is offered for an integer `BITPIX` **only** when `BSCALE` is 1 and
/// `BZERO` is the value that maps the signed storage type onto its unsigned range. Those are
/// the cases where physical values provably occupy `[0, 2ⁿ − 1]`. Any other pairing is refused
/// rather than normalized: a genuinely signed frame would otherwise have half its levels
/// saturate to black, and a rescaled frame would normalize to a sliver near zero. Both would
/// *look* like images and be wrong.
///
/// FITS defines no representable range for floats either. `DATAMIN`/`DATAMAX` are reported as
/// ordinary keywords and not consumed: they describe the range the data *occupies*, not the
/// range it is *displayed against*, and conflating the two would rescale every frame by its
/// own content.
fn fits_bounds(format: Option<SampleFormat>, bscale: f64, bzero: f64, declined: bool) -> Bounds {
    if declined {
        return Bounds::Unavailable(BoundsUnavailable::NoFormatDefault);
    }
    let Some(format) = format else {
        return Bounds::Unavailable(BoundsUnavailable::NoFormatDefault);
    };
    if !format.is_integer() || bscale != 1.0 {
        return Bounds::Unavailable(BoundsUnavailable::NoFormatDefault);
    }
    // 0 for BITPIX = 8, 32768 for 16, 2147483648 for 32, 2^63 for 64. The 64-bit value
    // exceeds i64::MAX and can only be parsed as a float, which is why the lexer must not
    // assume an integer-valued keyword fits an i64.
    let (expected_bzero, hi) = match format {
        SampleFormat::U8 => (0.0, f64::from(u8::MAX)),
        SampleFormat::I16 => (32768.0, f64::from(u16::MAX)),
        SampleFormat::I32 => (2_147_483_648.0, f64::from(u32::MAX)),
        SampleFormat::I64 => ((1u128 << 63) as f64, u64::MAX as f64),
        _ => return Bounds::Unavailable(BoundsUnavailable::NoFormatDefault),
    };
    if bzero == expected_bzero {
        Bounds::FormatDefault(0.0, hi)
    } else {
        Bounds::Unavailable(BoundsUnavailable::NoFormatDefault)
    }
}

// ------------------------------------------------------------------ sample decode

/// FITS pixel data is big-endian two's-complement, always.
///
/// The format carries no attribute to read and no default to infer, so this is pinned for the
/// same reason XISF's little-endian default is: getting it wrong corrupts every sample
/// silently rather than raising an error.
/// `raw` is one row, sized by the caller from the same geometry that sized `out`, so a short
/// `raw` is this crate's arithmetic contradicting itself rather than anything a file can cause.
/// The row is therefore taken whole up front — `chunks_exact` over a slice of exactly the
/// needed length — instead of the per-sample `get(..) else break` that stood here, which
/// truncated silently and left the tail of the row at whatever `Samples::zeroed` put there.
/// Fabricating zero pixels is the silent repair § The organizing principle refuses, and the
/// panic on the slice below is the same answer `slice_samples` and the XISF row decoder give
/// to the identical class of failure.
fn decode_be(raw: &[u8], out: &mut Samples) {
    macro_rules! fill {
        ($v:expr, $w:expr, $conv:expr) => {{
            let row = &raw[..$v.len() * $w];
            for (dst, bytes) in $v.iter_mut().zip(row.chunks_exact($w)) {
                let mut b = [0u8; $w];
                b.copy_from_slice(bytes);
                *dst = $conv(b);
            }
        }};
    }
    // Matching on the destination alone is what removes the "sample format disagrees with
    // the buffer" arm: there is no such state to handle, so there is no panic to write.
    match out {
        Samples::U8(v) => {
            let n = v.len();
            v.copy_from_slice(&raw[..n]);
        }
        Samples::U16(v) => fill!(v, 2, u16::from_be_bytes),
        Samples::U32(v) => fill!(v, 4, u32::from_be_bytes),
        Samples::U64(v) => fill!(v, 8, u64::from_be_bytes),
        Samples::I16(v) => fill!(v, 2, i16::from_be_bytes),
        Samples::I32(v) => fill!(v, 4, i32::from_be_bytes),
        Samples::I64(v) => fill!(v, 8, i64::from_be_bytes),
        Samples::F32(v) => fill!(v, 4, f32::from_be_bytes),
        Samples::F64(v) => fill!(v, 8, f64::from_be_bytes),
    }
}
