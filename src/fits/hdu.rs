//! What one HDU's cards mean: the sizing keywords, the decline table, the size formula and
//! the `Header` they assemble into.
//!
//! Everything here is a function of a keyword list. Nothing reads, seeks or holds a position,
//! which is what lets the size arithmetic — the part with no fixture-independent way to be
//! wrong — be exercised directly rather than only through whole files. The walk that supplies
//! the lists is [`crate::fits::decoder`].

use std::sync::Arc;

use crate::fits::cards::{BLOCK, lex_integer, lex_logical, lex_number};
use crate::header::{
    Bounds, BoundsUnavailable, DeclineClass, DeclineReason, Geometry, Granularity, Header,
    PixelStorage, RowOrder,
};
use crate::metadata::{Keyword, KeywordSet, PropertySet, ValueKind};
use crate::normalize::{Range, Scaling};
use crate::samples::SampleFormat;

/// The value text of the card named `name`, verbatim.
pub(crate) fn keyword_value<'a>(keywords: &'a [Keyword], name: &str) -> Option<&'a str> {
    keywords
        .iter()
        .find(|k| k.name() == name)
        .map(|k| k.value())
}

/// The value text of the card named `name`, **and only when the card wrote it as something
/// other than a character string**.
///
/// FITS 4.0 §4.2 gives every keyword a value *type*, and the structural keywords this crate
/// lexes — `SIMPLE`, `BITPIX`, `NAXIS`, `NAXISn`, `PCOUNT`, `GCOUNT`, `GROUPS`, `ZIMAGE`,
/// `BLANK`, `INHERIT`, `BSCALE`, `BZERO` — are logical, integer or real valued. A quoted
/// spelling of one of them is a character string that happens to contain the same characters,
/// so `BITPIX = '16'` is not the integer 16 and `SIMPLE = 'T'` does not assert conformance.
/// Reading either as though it were is the silent plausible repair § The organizing principle
/// refuses: the value text alone cannot tell the two apart, because unquoting has already
/// happened by the time it is read.
///
/// The keywords whose FITS-defined type **is** a character string — `XTENSION`, `ROWORDER` —
/// go through [`keyword_value`] instead. The distinction is the keyword's declared type, not
/// this function's preference.
pub(crate) fn structural_value<'a>(keywords: &'a [Keyword], name: &str) -> Option<&'a str> {
    keywords
        .iter()
        .find(|k| k.name() == name)
        .filter(|k| k.value_kind() != ValueKind::CharacterString)
        .map(|k| k.value())
}

/// The structural facts a walk needs, each carrying whether it read at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum UnitSize {
    /// `|BITPIX|/8 × GCOUNT × (PCOUNT + Π NAXISᵢ)`, rounded up to the block boundary.
    Bytes(u64),
    /// A missing, unparseable or out-of-standard-set sizing keyword, or arithmetic that
    /// overflowed. There is no resumption point, so the walk ends.
    Unsizable(&'static str),
}

/// The five sizing keywords, lexed **once** per HDU.
///
/// `BITPIX`, `NAXIS`, the `NAXISn` run, `PCOUNT` and `GCOUNT` are read by three consumers with
/// three different sets of rules — [`is_image_position`] asks only what `NAXIS` is,
/// [`first_fault`] assigns a decline class in the validation order § Errors fixes, and
/// [`data_unit_size`] decides whether the walk can step over the data unit at all. Keeping
/// those judgements apart is deliberate and § Errors → Validation order says why: the decline
/// table must stay independent of the size formula, so that a header carrying two faults
/// classifies determinately rather than by whichever function looked first.
///
/// **Sharing the lexing is not sharing the judgement.** What this type holds is what the cards
/// *said*, as data: a value that did not read is `None`, a negative one is a negative `i64`,
/// and no field here carries a class, a message or a verdict. Each consumer assigns its own.
/// Reading them separately is what let the same `NAXIS` be in range for one function and out
/// of it for another.
#[derive(Debug)]
pub(crate) struct Sizing {
    /// `None` when the card is missing, quoted, or not an integer value.
    bitpix: Option<i64>,
    /// `None` when the card is missing, quoted, or not an integer value.
    naxis: Option<i64>,
    /// `NAXIS1` onward in order, **truncated at the first one that did not read**.
    ///
    /// The truncation point is the fault, carried as a length rather than as a class: an index
    /// at or past `axes.len()` and below `naxis` is a `NAXISn` that is missing, quoted or
    /// unparseable, and the consumer says which of its own words that is. Stopping there is
    /// also what bounds the read — `NAXIS` is an `i64` and no card has to agree with it, so
    /// the run is bounded by the cards the header actually carries rather than by the number
    /// `NAXIS` names.
    axes: Vec<i64>,
    /// `None` when the card is missing, quoted, or not an integer value — which is the
    /// ordinary state of a primary header, where §4.4.1.1 defaults them.
    pcount: Option<i64>,
    gcount: Option<i64>,
}

impl Sizing {
    pub(crate) fn read(keywords: &[Keyword]) -> Sizing {
        let integer = |name: &str| structural_value(keywords, name).and_then(lex_integer);
        let naxis = integer("NAXIS");
        let mut axes: Vec<i64> = Vec::new();
        for i in 1..=naxis.unwrap_or(0) {
            let Some(v) = integer(&format!("NAXIS{i}")) else {
                break;
            };
            axes.push(v);
        }
        Sizing {
            bitpix: integer("BITPIX"),
            naxis,
            axes,
            pcount: integer("PCOUNT"),
            gcount: integer("GCOUNT"),
        }
    }

    /// `NAXIS{i}`'s value, or `None` when it did not read.
    fn axis(&self, i: i64) -> Option<i64> {
        let index = usize::try_from(i.checked_sub(1)?).ok()?;
        self.axes.get(index).copied()
    }
}

/// Whether the reader sits on this HDU or steps over it.
///
/// A primary with `NAXIS = 0` is the ordinary shape of every multi-extension file, so it is
/// not a decline at all — only the *absence of any image anywhere* is left, and that is end
/// of source rather than an error. An `XTENSION = 'IMAGE'` with `NAXIS = 0` answers the same
/// question the same way. A `BINTABLE` carrying `ZIMAGE = T` is the opposite: a declined
/// position rather than an aborted source, so the second-pass dispatch point has somewhere to
/// attach.
pub(crate) fn is_image_position(
    keywords: &[Keyword],
    xtension: Option<&str>,
    sizing: &Sizing,
) -> bool {
    match xtension {
        None | Some("IMAGE") => match sizing.naxis {
            // A missing or unparseable NAXIS is a declined image position rather than an
            // unsizable skip: the reader sits on it and reports what it can.
            None => true,
            Some(0) => false,
            Some(_) => true,
        },
        Some("BINTABLE") => {
            structural_value(keywords, "ZIMAGE").and_then(lex_logical) == Some(true)
        }
        _ => false,
    }
}

/// Read the geometry, if it reads at all.
///
/// The line is **representability, not validity**: a geometry this crate can read is reported
/// even when what it reads is what declines the position. So this is computed independently
/// of which fault wins the class.
///
/// Read from the cards rather than from [`Sizing`], because `prefix` is what a tile-compressed
/// `BINTABLE` needs: the geometry it reports comes from `ZNAXIS`/`ZNAXISn` while its data unit
/// is sized by the table's own `NAXIS`/`NAXISn`. The two runs are different cards, and `Sizing`
/// covers the one that sizes the data unit.
pub(crate) fn read_geometry(keywords: &[Keyword], prefix: &str) -> Option<Geometry> {
    let naxis = structural_value(keywords, &format!("{prefix}NAXIS")).and_then(lex_integer)?;
    let axis = |i: i64| -> Option<u32> {
        let v = structural_value(keywords, &format!("{prefix}NAXIS{i}")).and_then(lex_integer)?;
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

pub(crate) fn read_sample_format(keywords: &[Keyword], prefix: &str) -> Option<SampleFormat> {
    let bitpix = structural_value(keywords, &format!("{prefix}BITPIX")).and_then(lex_integer)?;
    sample_format_of(bitpix)
}

pub(crate) fn sample_format_of(bitpix: i64) -> Option<SampleFormat> {
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
///
/// The five sizing keywords arrive already lexed, and the classes below are this function's
/// alone — [`data_unit_size`] reads the same [`Sizing`] and reaches its own verdicts in its
/// own order.
pub(crate) fn first_fault(
    keywords: &[Keyword],
    xtension: Option<&str>,
    sizing: &Sizing,
) -> Option<DeclineReason> {
    let is_primary = xtension.is_none();

    // BITPIX
    match sizing.bitpix {
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
    let naxis = match sizing.naxis {
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
        match sizing.axis(i) {
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
        for (name, value) in [("PCOUNT", sizing.pcount), ("GCOUNT", sizing.gcount)] {
            if value.is_none() {
                return Some(DeclineReason::new(
                    DeclineClass::Malformed,
                    format!("{name} is missing or is not an integer value in an extension header"),
                ));
            }
        }
    }

    // GROUPS
    if structural_value(keywords, "GROUPS").and_then(lex_logical) == Some(true) {
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
    if xtension == Some("BINTABLE") {
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
        match sizing.axis(i) {
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
        if let Some(v) = structural_value(keywords, name)
            && lex_number(v).is_none()
        {
            return Some(DeclineReason::new(
                DeclineClass::Malformed,
                format!("{name} = {v:?} is not a numeric value"),
            ));
        }
    }
    if let Some(v) = structural_value(keywords, "BLANK")
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
///
/// **Every reason this returns `Unsizable` for is one § Errors enumerates**: a missing,
/// unparseable or out-of-standard-set `BITPIX`; a missing or unparseable `NAXIS`, `NAXISn`,
/// `PCOUNT` or `GCOUNT`; a value of one of them that is negative, which is a size the
/// arithmetic has no reading for; or arithmetic that overflows the `u64` it runs in. A
/// `NAXIS` this crate declines for *scope* is not among them and is not refused here —
/// scope belongs to [`first_fault`], and an `NAXIS` above 999 needs no bound of its own,
/// `NAXIS1000` not being a name an eight-byte keyword field can hold: the axis run reaches a
/// `NAXISn` that is not there and returns the enumerated fault instead.
pub(crate) fn data_unit_size(keywords: &[Keyword], sizing: &Sizing, is_primary: bool) -> UnitSize {
    let Some(bitpix) = sizing.bitpix else {
        return UnitSize::Unsizable("BITPIX is missing or is not an integer value");
    };
    if sample_format_of(bitpix).is_none() {
        return UnitSize::Unsizable("BITPIX is outside the standard set");
    }
    let Some(naxis) = sizing.naxis else {
        return UnitSize::Unsizable("NAXIS is missing or is not an integer value");
    };
    if naxis < 0 {
        // Not merely out of scope: the axis run below is empty for a negative `NAXIS`, so
        // without this the unit would size as though the header declared no axes at all.
        return UnitSize::Unsizable("NAXIS is negative");
    }
    if naxis == 0 {
        // §7.1.1: no data blocks follow, with PCOUNT zero and GCOUNT one. The skip is exact
        // rather than estimated, so it costs no `Skipped block bytes` at all.
        return UnitSize::Bytes(0);
    }

    let random_groups =
        is_primary && structural_value(keywords, "GROUPS").and_then(lex_logical) == Some(true);
    let first_axis = if random_groups { 2 } else { 1 };

    // The product of no axes is one everywhere except here: a random-groups header with
    // `NAXIS = 1` has no group data array at all, so each group is its parameters and nothing
    // else, and the term §6.1.1 adds `PCOUNT` to is zero rather than one.
    let mut elements: u64 = u64::from(!(random_groups && naxis < 2));
    for i in first_axis..=naxis {
        let Some(v) = sizing.axis(i) else {
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
        let Some(p) = sizing.pcount else {
            return UnitSize::Unsizable("PCOUNT is missing or is not an integer value");
        };
        let Some(g) = sizing.gcount else {
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

pub(crate) fn build_header(
    keywords: &Arc<[Keyword]>,
    xtension: Option<&str>,
    sizing: &Sizing,
    primary_reported: &Arc<[Keyword]>,
    primary_row_order: &RowOrder,
) -> Header {
    let is_primary = xtension.is_none();
    let tile_compressed = xtension == Some("BINTABLE");
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
    let keyword_set = KeywordSet::new(
        keywords.clone(),
        if is_primary {
            // A primary header inherits from nothing, so there is no second piece.
            Arc::default()
        } else {
            primary_reported.clone()
        },
    );

    let applied = |name: &str| -> Option<&str> {
        if let Some(v) = structural_value(keywords, name) {
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
        if structural_value(keywords, "INHERIT").and_then(lex_logical) != Some(true) {
            return None;
        }
        // The reported list, not a second copy of it: `reorigin` rewrites a card's origin
        // and nothing else, and this lookup reads a card's name, its value and its value
        // kind — none of which the re-tagging touches.
        structural_value(primary_reported, name)
    };

    let geometry = read_geometry(keywords, prefix);
    let sample_format = read_sample_format(keywords, prefix);
    let decline_reason = first_fault(keywords, xtension, sizing);

    let bscale = applied("BSCALE").and_then(lex_number).unwrap_or(1.0);
    let bzero = applied("BZERO").and_then(lex_number).unwrap_or(0.0);
    // `applied`'s rule, spelled out rather than called, because `ROWORDER` is the one
    // inheritable card whose *text* is reported: the other three are lexed to numbers. The
    // inherited value is classified once for the whole source and cloned here — see
    // `Decoder::primary_row_order` for what building it per position cost.
    let row_order = if is_primary {
        primary_row_order.clone()
    } else if let Some(text) = keyword_value(keywords, "ROWORDER") {
        // The extension's own card, which is that extension's own input bytes.
        RowOrder::classify(text)
    } else if structural_value(keywords, "INHERIT").and_then(lex_logical) == Some(true) {
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
        keywords: keyword_set,
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
    let expected_bzero = match format {
        SampleFormat::U8 => 0.0,
        SampleFormat::I16 => 32768.0,
        SampleFormat::I32 => 2_147_483_648.0,
        SampleFormat::I64 => (1u128 << 63) as f64,
        _ => return Bounds::Unavailable(BoundsUnavailable::NoFormatDefault),
    };
    if bzero != expected_bzero {
        return Bounds::Unavailable(BoundsUnavailable::NoFormatDefault);
    }
    // The `[0, 2ⁿ − 1]` range for the width the physical values occupy, built through the one
    // constructor that computes it — the same call the XISF default takes, so the two formats
    // cannot state the same range in two spellings.
    match Range::unsigned_default(format.bytes() * 8) {
        Some(range) => Bounds::FormatDefault(range),
        None => Bounds::Unavailable(BoundsUnavailable::NoFormatDefault),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::KeywordOrigin;

    /// One valued card, as an unquoted value field — the kind every structural keyword has.
    fn card(name: &str, value: &str) -> Keyword {
        Keyword::new(name, value, None, KeywordOrigin::Image, ValueKind::Other)
    }

    /// The same card written as a character string: `BITPIX = '16'`.
    fn quoted(name: &str, value: &str) -> Keyword {
        Keyword::new(
            name,
            value,
            None,
            KeywordOrigin::Image,
            ValueKind::CharacterString,
        )
    }

    fn cards(pairs: &[(&str, &str)]) -> Vec<Keyword> {
        pairs.iter().map(|(n, v)| card(n, v)).collect()
    }

    /// A minimal sizable primary: 4 × 2 samples of `BITPIX = 16`, one block of data.
    fn primary() -> Vec<Keyword> {
        cards(&[
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "4"),
            ("NAXIS2", "2"),
        ])
    }

    fn size(keywords: &[Keyword], is_primary: bool) -> UnitSize {
        data_unit_size(keywords, &Sizing::read(keywords), is_primary)
    }

    fn fault(keywords: &[Keyword], xtension: Option<&str>) -> Option<DeclineReason> {
        first_fault(keywords, xtension, &Sizing::read(keywords))
    }

    // -------------------------------------------------------------- the size formula

    #[test]
    fn a_sizable_primary_pads_to_the_block_boundary() {
        // 4 × 2 × 2 bytes = 16, one 2880-byte block.
        assert_eq!(size(&primary(), true), UnitSize::Bytes(2880));
    }

    #[test]
    fn a_naxis_zero_unit_is_exactly_zero_bytes() {
        let kw = cards(&[("BITPIX", "8"), ("NAXIS", "0")]);
        assert_eq!(size(&kw, true), UnitSize::Bytes(0));
    }

    /// §6.1.1: under `GROUPS = T` the axis product runs over `NAXIS2`…`NAXISn`, so a
    /// random-groups header with `NAXIS = 1` has no group data array at all and each group is
    /// its parameters alone. The product of no axes is one everywhere else; here it is zero,
    /// and getting that wrong adds one element per group to the step.
    #[test]
    fn random_groups_with_naxis_one_sizes_the_parameters_alone() {
        let kw = cards(&[
            ("BITPIX", "16"),
            ("NAXIS", "1"),
            ("NAXIS1", "0"),
            ("GROUPS", "T"),
            ("PCOUNT", "3"),
            ("GCOUNT", "5"),
        ]);
        // (0 + 3) × 5 × 2 = 30 bytes, one block.
        assert_eq!(size(&kw, true), UnitSize::Bytes(2880));

        // The same header with no parameters per group holds nothing at all. Read with the
        // product taken as one it would be (1 + 0) × 5 × 2 = 10 bytes and a whole block of
        // step, which lands the walk a block past where the next header actually starts.
        let empty = cards(&[
            ("BITPIX", "16"),
            ("NAXIS", "1"),
            ("NAXIS1", "0"),
            ("GROUPS", "T"),
            ("PCOUNT", "0"),
            ("GCOUNT", "5"),
        ]);
        assert_eq!(size(&empty, true), UnitSize::Bytes(0));
    }

    /// The random-groups exception is the primary's alone: an extension carrying `GROUPS = T`
    /// is sized the ordinary way, `NAXIS1` included.
    #[test]
    fn groups_in_an_extension_does_not_move_the_axis_product() {
        let kw = cards(&[
            ("BITPIX", "8"),
            ("NAXIS", "2"),
            ("NAXIS1", "0"),
            ("NAXIS2", "3"),
            ("GROUPS", "T"),
            ("PCOUNT", "0"),
            ("GCOUNT", "1"),
        ]);
        // Π NAXISᵢ includes NAXIS1 = 0 here, so the unit is empty. Read as a primary the
        // product would skip NAXIS1 and give 3 elements, and a block of step with it.
        assert_eq!(size(&kw, false), UnitSize::Bytes(0));
        assert_eq!(size(&kw, true), UnitSize::Bytes(2880));
    }

    /// `PCOUNT` carries a `BINTABLE`'s heap, which is what the naive `BITPIX × NAXIS*` form
    /// loses: the step lands mid-heap and every HDU after it misparses.
    #[test]
    fn pcount_adds_the_heap_to_an_extension_data_unit() {
        let kw = cards(&[
            ("BITPIX", "8"),
            ("NAXIS", "2"),
            ("NAXIS1", "10"),
            ("NAXIS2", "288"),
            ("PCOUNT", "2880"),
            ("GCOUNT", "1"),
        ]);
        // 2880 table bytes + 2880 heap bytes = two blocks; without PCOUNT it is one.
        assert_eq!(size(&kw, false), UnitSize::Bytes(5760));
    }

    #[test]
    fn a_primary_defaults_pcount_and_gcount_rather_than_requiring_them() {
        assert_eq!(size(&primary(), true), UnitSize::Bytes(2880));
        // The same cards in an extension header need both, and say which is missing first.
        assert_eq!(
            size(&primary(), false),
            UnitSize::Unsizable("PCOUNT is missing or is not an integer value")
        );
    }

    // -------------------------------------------------------------- the overflow returns

    #[test]
    fn the_axis_product_overflow_is_reported_rather_than_wrapped() {
        let max = u32::MAX.to_string();
        let kw = cards(&[
            ("BITPIX", "8"),
            ("NAXIS", "3"),
            ("NAXIS1", &max),
            ("NAXIS2", &max),
            ("NAXIS3", &max),
        ]);
        // Three u32 axes reach 2^96, which no u64 holds.
        assert_eq!(
            size(&kw, true),
            UnitSize::Unsizable("the axis product overflows u64")
        );
    }

    /// The second overflow return: a product that fits `u64` and a `× GCOUNT × width` that
    /// does not. Reachable only in an extension, `GCOUNT` being 1 in the primary.
    #[test]
    fn the_size_arithmetic_overflow_is_reported_rather_than_wrapped() {
        let kw = cards(&[
            ("BITPIX", "64"),
            ("NAXIS", "1"),
            ("NAXIS1", &(u32::MAX as u64 * 2).to_string()),
            ("PCOUNT", "0"),
            ("GCOUNT", &(u32::MAX as u64 * 2).to_string()),
        ]);
        assert_eq!(
            size(&kw, false),
            UnitSize::Unsizable("the data-unit size arithmetic overflows u64")
        );
    }

    /// The third: a size that fits `u64` and whose padding to the next block boundary does
    /// not. `i64::MAX × 2` bytes is 40 short of `u64::MAX`, and the pad adds 2879.
    #[test]
    fn padding_to_a_block_boundary_reports_its_own_overflow() {
        let kw = cards(&[
            ("BITPIX", "16"),
            ("NAXIS", "1"),
            ("NAXIS1", &(i64::MAX - 19).to_string()),
            ("PCOUNT", "0"),
            ("GCOUNT", "1"),
        ]);
        assert_eq!(
            size(&kw, false),
            UnitSize::Unsizable("padding the data-unit size to a block boundary overflows u64")
        );
    }

    #[test]
    fn a_negative_naxis_is_unsizable_rather_than_sized_as_no_axes() {
        let kw = cards(&[("BITPIX", "8"), ("NAXIS", "-1")]);
        assert_eq!(size(&kw, true), UnitSize::Unsizable("NAXIS is negative"));
    }

    #[test]
    fn a_negative_axis_is_unsizable() {
        let kw = cards(&[("BITPIX", "8"), ("NAXIS", "1"), ("NAXIS1", "-4")]);
        assert_eq!(size(&kw, true), UnitSize::Unsizable("a NAXISn is negative"));
    }

    /// `NAXIS = 1000` has no bound of its own: `NAXIS1000` is not a name an eight-byte keyword
    /// field can hold, so the axis run reaches a `NAXISn` that is not there and returns the
    /// reason § Errors enumerates rather than one about the standard's 999.
    #[test]
    fn an_out_of_range_naxis_faults_on_the_axis_it_cannot_find() {
        let kw = cards(&[("BITPIX", "8"), ("NAXIS", "1000"), ("NAXIS1", "2")]);
        assert_eq!(
            size(&kw, true),
            UnitSize::Unsizable("a NAXISn is missing or is not an integer value")
        );
        // And the decline table calls the same header out of scope, in its own words.
        let reason = fault(&kw, None).expect("a header declaring 1000 axes is declined");
        assert_eq!(reason.class(), DeclineClass::Malformed);
        assert!(reason.reason().contains("NAXIS2"), "{reason:?}");
    }

    /// The axis run stops at the first `NAXISn` that does not read, so a `NAXIS` no header
    /// could satisfy costs the cards it carries rather than the number it names.
    #[test]
    fn the_axis_run_is_bounded_by_the_cards_rather_than_by_naxis() {
        let kw = cards(&[("BITPIX", "8"), ("NAXIS", &i64::MAX.to_string())]);
        assert_eq!(Sizing::read(&kw).axes.len(), 0);
        assert_eq!(
            size(&kw, true),
            UnitSize::Unsizable("a NAXISn is missing or is not an integer value")
        );
    }

    // -------------------------------------------------------------- one read, two judgements

    /// The decline table and the size formula read one [`Sizing`] and reach different
    /// verdicts about the same cards — which is the point of keeping them apart. A `PCOUNT`
    /// missing from an extension is `Malformed` at the position and unsizable for the walk;
    /// a `NAXIS` of 4 is in scope for neither but sizes perfectly well.
    #[test]
    fn the_decline_table_and_the_size_formula_judge_the_same_cards_separately() {
        let kw = cards(&[
            ("BITPIX", "8"),
            ("NAXIS", "4"),
            ("NAXIS1", "2"),
            ("NAXIS2", "2"),
            ("NAXIS3", "2"),
            ("NAXIS4", "2"),
            ("PCOUNT", "0"),
            ("GCOUNT", "1"),
        ]);
        let sizing = Sizing::read(&kw);
        let reason = first_fault(&kw, Some("IMAGE"), &sizing).expect("NAXIS = 4 is out of scope");
        assert_eq!(reason.class(), DeclineClass::Unsupported);
        assert_eq!(data_unit_size(&kw, &sizing, false), UnitSize::Bytes(2880));
    }

    // -------------------------------------------------------------- the value kind

    /// A structural keyword written as a character string is not that keyword's value.
    /// `BITPIX = '16'` is a string that spells a number, and reading it as 16 is the silent
    /// repair § The organizing principle refuses.
    #[test]
    fn a_quoted_structural_value_is_refused() {
        let mut kw = primary();
        kw[0] = quoted("BITPIX", "16");
        assert_eq!(
            size(&kw, true),
            UnitSize::Unsizable("BITPIX is missing or is not an integer value")
        );
        let reason = fault(&kw, None).expect("a quoted BITPIX declines the position");
        assert_eq!(reason.class(), DeclineClass::Malformed);
        assert!(reason.reason().contains("BITPIX"), "{reason:?}");
        // The card is still reported verbatim: the value text is what the file wrote, and the
        // kind beside it is what says the file wrote it in quotes.
        assert_eq!(kw[0].value(), "16");
        assert_eq!(kw[0].value_kind(), ValueKind::CharacterString);
    }

    #[test]
    fn a_quoted_axis_declines_the_position_it_would_have_sized() {
        let mut kw = primary();
        kw[2] = quoted("NAXIS1", "4");
        assert_eq!(read_geometry(&kw, ""), None);
        let reason = fault(&kw, None).expect("a quoted NAXIS1 declines the position");
        assert_eq!(reason.class(), DeclineClass::Malformed);
        assert!(reason.reason().contains("NAXIS1"), "{reason:?}");
    }

    /// `XTENSION` and `ROWORDER` are character-string valued by the standard and by the
    /// convention respectively, so the quoted spelling is the conforming one for them and the
    /// strictness above must not reach it.
    #[test]
    fn a_string_valued_keyword_reads_from_its_quoted_spelling() {
        let kw = vec![quoted("ROWORDER", "BOTTOM-UP")];
        assert_eq!(keyword_value(&kw, "ROWORDER"), Some("BOTTOM-UP"));
        assert_eq!(structural_value(&kw, "ROWORDER"), None);
    }

    // -------------------------------------------------------------- geometry and format

    #[test]
    fn geometry_reads_a_cube_as_channels_and_declines_other_ranks() {
        let kw = cards(&[
            ("NAXIS", "3"),
            ("NAXIS1", "4"),
            ("NAXIS2", "2"),
            ("NAXIS3", "3"),
        ]);
        assert_eq!(
            read_geometry(&kw, ""),
            Some(Geometry {
                width: 4,
                height: 2,
                channels: 3,
            })
        );
        let flat = cards(&[("NAXIS", "1"), ("NAXIS1", "4")]);
        assert_eq!(read_geometry(&flat, ""), None);
    }

    /// A tile-compressed `BINTABLE` reports the geometry its `Z*` keywords declare while its
    /// data unit is sized by the table's own `NAXIS*` — two runs of cards that must not be
    /// read from the same place.
    #[test]
    fn the_z_prefix_reads_a_second_geometry_beside_the_tables_own() {
        let kw = cards(&[
            ("BITPIX", "8"),
            ("NAXIS", "2"),
            ("NAXIS1", "12"),
            ("NAXIS2", "2"),
            ("PCOUNT", "0"),
            ("GCOUNT", "1"),
            ("ZIMAGE", "T"),
            ("ZBITPIX", "16"),
            ("ZNAXIS", "2"),
            ("ZNAXIS1", "6"),
            ("ZNAXIS2", "4"),
        ]);
        assert_eq!(
            read_geometry(&kw, "Z"),
            Some(Geometry {
                width: 6,
                height: 4,
                channels: 1,
            })
        );
        assert_eq!(read_sample_format(&kw, "Z"), Some(SampleFormat::I16));
        assert_eq!(size(&kw, false), UnitSize::Bytes(2880));
        assert!(is_image_position(&kw, Some("BINTABLE"), &Sizing::read(&kw)));
    }

    // -------------------------------------------------------------- the range

    #[test]
    fn only_the_unsigned_convention_gets_a_format_default_range() {
        assert!(matches!(
            fits_bounds(Some(SampleFormat::I16), 1.0, 32768.0, false),
            Bounds::FormatDefault(range) if range.lo() == 0.0 && range.hi() == 65535.0
        ));
        // The signed-byte convention is its mirror image and is not it.
        assert!(matches!(
            fits_bounds(Some(SampleFormat::U8), 1.0, -128.0, false),
            Bounds::Unavailable(BoundsUnavailable::NoFormatDefault)
        ));
        // A rescaled frame is refused rather than normalized against a range it does not have.
        assert!(matches!(
            fits_bounds(Some(SampleFormat::I16), 2.0, 32768.0, false),
            Bounds::Unavailable(BoundsUnavailable::NoFormatDefault)
        ));
        // So is every float frame, FITS declaring no range for one.
        assert!(matches!(
            fits_bounds(Some(SampleFormat::F32), 1.0, 0.0, false),
            Bounds::Unavailable(BoundsUnavailable::NoFormatDefault)
        ));
    }
}
