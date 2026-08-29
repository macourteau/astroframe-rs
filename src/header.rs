//! Everything parsed before pixels. The user-facing prose lives on [`Header`] itself, which
//! is where a caller meets it.

use std::sync::Arc;

use crate::error::Error;
use crate::metadata::{
    Cfa, DisplayFunction, Keyword, KeywordSet, Keywords, Properties, Property, PropertySet,
    Resolution,
};
use crate::normalize::{SampleRange, Scaling};
use crate::samples::SampleFormat;

/// FITS `ROWORDER`, reported and applied to nothing.
///
/// `ROWORDER` is a widely-used convention and is **not** in FITS Standard 4.0, so
/// unrecognized values are expected and [`RowOrder::Other`] reports them verbatim.
/// Recognition is case-insensitive and trims surrounding whitespace — that normalization is
/// a correctness rule rather than a convenience, being what stops a consumer's envelope
/// predicate from admitting a flipped frame through a spelling variant.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RowOrder {
    /// `TOP-DOWN`.
    TopDown,
    /// `BOTTOM-UP`.
    BottomUp,
    /// The keyword was absent — a different fact from a declared `TOP-DOWN`, since the
    /// convention is one a file may simply not have invoked.
    Unspecified,
    /// A value this crate does not recognize, verbatim.
    ///
    /// Shared rather than owned, for the reason [`Orientation::Other`] is, and reached by a
    /// different route: a `ROWORDER` card in the primary header applies to every extension
    /// under `INHERIT = T`, and its value is a *keyword* value, bounded by
    /// `Assembled keyword value` rather than by the eighty bytes of one card — a `CONTINUE`
    /// chain assembles it. Classified per image position it cost 189 MB, 64 MB of it held
    /// live, from a 1.06 MB input at 256 extensions. The `Decoder` classifies the primary's
    /// once for the source and shares it, which needs the payload to be shareable.
    Other(Arc<str>),
}

impl RowOrder {
    /// Classify a `ROWORDER` value.
    #[must_use]
    pub fn classify(value: &str) -> RowOrder {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("TOP-DOWN") {
            RowOrder::TopDown
        } else if trimmed.eq_ignore_ascii_case("BOTTOM-UP") {
            RowOrder::BottomUp
        } else {
            RowOrder::Other(Arc::from(trimmed))
        }
    }

    /// The spelling a file writes, which is what a consumer re-emitting the fact needs, and
    /// `None` for [`RowOrder::Unspecified`], which has no spelling because no card carried it.
    ///
    /// The two recognized values report the convention's own upper-case spelling rather than
    /// whatever case the file used; [`RowOrder::Other`] reports its payload verbatim.
    ///
    /// **`Option<&str>` here, a bare `&str` on [`Orientation`], [`ImageType`] and
    /// [`ColorSpace`] — because the facts differ, not from inconsistency.** Every value of
    /// those three has a wire spelling, so there is nothing for an `Option` to report. This
    /// enum has one value that is the *absence* of a card, and a bare `&str` would have to
    /// answer it with `""` — which is a spelling a file can write: `classify` trims, so a
    /// `ROWORDER` card holding only spaces is `Other("")`. Two different files would then be
    /// indistinguishable through the re-emission surface of a crate whose premise is
    /// reporting what the file says.
    ///
    /// ```
    /// use astroframe::RowOrder;
    ///
    /// assert_eq!(RowOrder::Unspecified.as_str(), None);
    /// assert_eq!(RowOrder::classify("   ").as_str(), Some(""));
    /// ```
    pub fn as_str(&self) -> Option<&str> {
        match self {
            RowOrder::TopDown => Some("TOP-DOWN"),
            RowOrder::BottomUp => Some("BOTTOM-UP"),
            RowOrder::Unspecified => None,
            RowOrder::Other(text) => Some(text),
        }
    }
}

/// [`RowOrder::Unspecified`] renders as the empty string, the two states
/// [`RowOrder::as_str`] separates being one line of output either way.
impl std::fmt::Display for RowOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str().unwrap_or(""))
    }
}

/// XISF `orientation` (§11.5.2), reported and applied to nothing.
///
/// §11.5.2 says a decoder claiming orientation support *should* apply the transform for
/// images "to be represented visually" and *must not* apply it to images loaded for
/// processing that depends on the physical disposition of pixels. This crate is squarely the
/// second case.
///
/// An image with no `orientation` attribute reports [`Orientation::Identity`] rather than a
/// distinct "absent" state, and the asymmetry with [`RowOrder::Unspecified`] is deliberate:
/// `0` and absence mean the same thing for a spec attribute with a defined default, whereas
/// an absent `ROWORDER` genuinely differs from a declared `TOP-DOWN`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Orientation {
    /// Wire spelling `0`.
    Identity,
    /// Wire spelling `flip`.
    Flip,
    /// Wire spelling `90`.
    Rotate90,
    /// Wire spelling `90;flip`.
    Rotate90Flip,
    /// Wire spelling `-90`.
    Rotate270,
    /// Wire spelling `-90;flip`.
    Rotate270Flip,
    /// Wire spelling `180`.
    Rotate180,
    /// Wire spelling `180;flip`.
    Rotate180Flip,
    /// A value this crate does not recognize, verbatim. `orientation` is a closed
    /// enumeration, but decoding does not depend on it, so an unknown value degrades to
    /// "unknown" and is reported rather than being a hard error.
    ///
    /// Shared rather than owned, for the reason [`Header::image_id`] is: this is an XML
    /// attribute value on an `<Image>`, and an image reached by N root-level `<Reference>`
    /// elements carries it once per occurrence. At the `Attribute value length` cap and 256
    /// references that was 276 MB retained from a one-megabyte header.
    Other(Arc<str>),
}

impl Orientation {
    /// Classify an `orientation` attribute's value.
    #[must_use]
    pub fn classify(value: &str) -> Orientation {
        match value {
            "0" => Orientation::Identity,
            "flip" => Orientation::Flip,
            "90" => Orientation::Rotate90,
            "90;flip" => Orientation::Rotate90Flip,
            "-90" => Orientation::Rotate270,
            "-90;flip" => Orientation::Rotate270Flip,
            "180" => Orientation::Rotate180,
            "180;flip" => Orientation::Rotate180Flip,
            other => Orientation::Other(Arc::from(other)),
        }
    }

    /// The §11.5.2 spelling a file writes, which is what a consumer re-emitting the attribute
    /// needs. [`Orientation::Other`] reports its payload verbatim.
    ///
    /// ```
    /// use astroframe::Orientation;
    ///
    /// assert_eq!(Orientation::classify("90;flip").as_str(), "90;flip");
    /// ```
    pub fn as_str(&self) -> &str {
        match self {
            Orientation::Identity => "0",
            Orientation::Flip => "flip",
            Orientation::Rotate90 => "90",
            Orientation::Rotate90Flip => "90;flip",
            Orientation::Rotate270 => "-90",
            Orientation::Rotate270Flip => "-90;flip",
            Orientation::Rotate180 => "180",
            Orientation::Rotate180Flip => "180;flip",
            Orientation::Other(text) => text,
        }
    }
}

impl std::fmt::Display for Orientation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// XISF `colorSpace` (§11.5.2 Table 14), reported. No conversion is performed.
///
/// Decoding depends on this enumeration, so an unrecognized spelling is a hard error rather
/// than an `Other` variant. `CIELab` is recognized and declined — [`Error::Unsupported`] —
/// which is a different outcome from a spelling the specification does not define.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ColorSpace {
    /// `Gray`. The default when the attribute is absent — never inferred from channel count.
    #[default]
    Gray,
    /// `RGB`.
    Rgb,
    /// `CIELab`. Recognized, and declined by this version.
    CieLab,
}

impl ColorSpace {
    /// The §11.5.2 Table 14 spelling a file writes.
    pub fn as_str(&self) -> &'static str {
        match self {
            ColorSpace::Gray => "Gray",
            ColorSpace::Rgb => "RGB",
            ColorSpace::CieLab => "CIELab",
        }
    }
}

impl std::fmt::Display for ColorSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// XISF `pixelStorage` (§11.5.2 Table 13), reported.
///
/// Decoding depends on this enumeration, so an unrecognized spelling is a hard error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum PixelStorage {
    /// Channel-planar. The default when the attribute is absent.
    #[default]
    Planar,
    /// Interleaved. Does not change granularity — every input row yields samples for all
    /// channels, so the decoder never has to hold more of the *input*; the cost is one
    /// row-sized staging buffer, not an image-sized one.
    Normal,
}

impl PixelStorage {
    /// The §11.5.2 Table 13 spelling a file writes.
    ///
    /// A bare `&str` rather than the `Option` [`RowOrder::as_str`] returns: both values are
    /// spellings the table defines, so there is no state here that means "the file wrote
    /// nothing". A FITS frame reports [`PixelStorage::Planar`], which is the layout FITS 4.0
    /// defines rather than an attribute it carries, and the spelling names that layout in the
    /// one vocabulary this crate has for it.
    pub fn as_str(&self) -> &'static str {
        match self {
            PixelStorage::Planar => "Planar",
            PixelStorage::Normal => "Normal",
        }
    }
}

impl std::fmt::Display for PixelStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// XISF `imageType` (§11.5.1 Table 12), reported.
///
/// Decoding does not depend on it, so an unrecognized value degrades to
/// [`ImageType::Other`] and is reported as text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[allow(missing_docs)] // one line per specification value would say only the value again
pub enum ImageType {
    Bias,
    Dark,
    Flat,
    Light,
    MasterBias,
    MasterDark,
    MasterFlat,
    MasterLight,
    DefectMap,
    RejectionMapHigh,
    RejectionMapLow,
    BinaryRejectionMapHigh,
    BinaryRejectionMapLow,
    SlopeMap,
    WeightMap,
    /// A value this crate does not recognize, verbatim. Shared rather than owned, for the
    /// reason [`Orientation::Other`] is.
    Other(Arc<str>),
}

impl ImageType {
    /// Classify an `imageType` attribute's value.
    #[must_use]
    pub fn classify(value: &str) -> ImageType {
        use ImageType as T;
        match value {
            "Bias" => T::Bias,
            "Dark" => T::Dark,
            "Flat" => T::Flat,
            "Light" => T::Light,
            "MasterBias" => T::MasterBias,
            "MasterDark" => T::MasterDark,
            "MasterFlat" => T::MasterFlat,
            "MasterLight" => T::MasterLight,
            "DefectMap" => T::DefectMap,
            "RejectionMapHigh" => T::RejectionMapHigh,
            "RejectionMapLow" => T::RejectionMapLow,
            "BinaryRejectionMapHigh" => T::BinaryRejectionMapHigh,
            "BinaryRejectionMapLow" => T::BinaryRejectionMapLow,
            "SlopeMap" => T::SlopeMap,
            "WeightMap" => T::WeightMap,
            other => T::Other(Arc::from(other)),
        }
    }

    /// The §11.5.1 Table 12 spelling a file writes. [`ImageType::Other`] reports its payload
    /// verbatim.
    pub fn as_str(&self) -> &str {
        use ImageType as T;
        match self {
            T::Bias => "Bias",
            T::Dark => "Dark",
            T::Flat => "Flat",
            T::Light => "Light",
            T::MasterBias => "MasterBias",
            T::MasterDark => "MasterDark",
            T::MasterFlat => "MasterFlat",
            T::MasterLight => "MasterLight",
            T::DefectMap => "DefectMap",
            T::RejectionMapHigh => "RejectionMapHigh",
            T::RejectionMapLow => "RejectionMapLow",
            T::BinaryRejectionMapHigh => "BinaryRejectionMapHigh",
            T::BinaryRejectionMapLow => "BinaryRejectionMapLow",
            T::SlopeMap => "SlopeMap",
            T::WeightMap => "WeightMap",
            T::Other(text) => text,
        }
    }
}

impl std::fmt::Display for ImageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which container a source is.
///
/// Reported for the reason everything else here is reported: which format a file is, is a
/// **fact about the file**, and § The organizing principle makes reporting a fact this crate
/// has already established the rule. The reader established it at construction, by sniffing
/// the leading bytes; without an accessor a caller reconstructs it from a format-specific
/// accessor's `Option` — `scaling().is_some()` is the usual one — which is an inference from
/// a fact reported for another purpose.
///
/// Neither variant is ever *produced* in a build with both format features disabled, because
/// no source is decoded there at all — every constructor returns `Unsupported` first. The
/// variants remain constructible by a caller, and the type is present across the whole
/// feature powerset, so code matching on it compiles in every configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// FITS, identified by the `SIMPLE` card the standard opens a file with.
    Fits,
    /// XISF, identified by the `XISF0100` signature of §6.1.
    Xisf,
}

impl Format {
    /// The format's name, as its own specification spells it.
    ///
    /// **Not a wire spelling, unlike the sibling `as_str` methods** — a container does not
    /// carry its name as a value to be re-emitted; it identifies itself by a signature. This
    /// is the string a log line, a report or a filter wants, and it is stable.
    ///
    /// ```
    /// use astroframe::Format;
    ///
    /// assert_eq!(Format::Xisf.as_str(), "XISF");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Fits => "FITS",
            Format::Xisf => "XISF",
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why no usable representable range exists.
///
/// The two raise different classes from `read_image_into`, so a caller holding an
/// [`Bounds::Unavailable`] must be able to tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BoundsUnavailable {
    /// The format defines no representable range for this source — a FITS float frame, or
    /// FITS integer scaling outside the unsigned convention. `read_image_into` raises
    /// [`Error::Unsupported`].
    NoFormatDefault,
    /// The image declared a `bounds` that is missing where it is mandatory, or that fails
    /// the range validity rule. Applies to **integer** images too, not just float ones.
    /// `read_image_into` raises [`Error::Malformed`].
    InvalidDeclared,
}

/// The representable range actually in force, and where it came from.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Bounds {
    /// No usable range exists, and this carries which.
    ///
    /// Such a frame is **not** rejected: its native samples decode normally through layer 1
    /// and only the normalized output is refused, with `set_bounds` as the escape hatch.
    Unavailable(BoundsUnavailable),
    /// The format's own default applied.
    ///
    /// Carries the validated [`SampleRange`], so a tier-3 caller normalizing chunks with the public
    /// primitive builds a [`Normalizer`](crate::Normalizer) from it directly — no re-deriving
    /// it from the sample width, and no fallible reconstruction of a range this crate has
    /// already checked. The endpoints stay reachable as [`SampleRange::lo`] and [`SampleRange::hi`].
    FormatDefault(SampleRange),
    /// The file stated this range.
    Declared(SampleRange),
    /// `set_bounds` overrode whatever the file said.
    CallerSupplied {
        /// The range in force.
        effective: SampleRange,
        /// What the file stated — its own text verbatim whenever it declared a `bounds` at
        /// all, usable or not, and `None` only when it declared none. So an override never
        /// erases the evidence, and overriding an override erases none either.
        ///
        /// Text rather than a numeric pair because an unparseable declaration is exactly the
        /// case this field exists to preserve, and a numeric pair cannot represent one.
        declared: Option<String>,
    },
}

/// How much of the input the decoder must hold before it can produce any sample.
///
/// It says nothing about how large a chunk the caller receives, and nothing about where in
/// the destination those samples land; conflating any of the three is the easy mistake here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Granularity {
    /// Row by row.
    Rows,
    /// One subblock at a time.
    Block {
        /// The number of independently-deliverable pieces — the only thing a caller can act
        /// on.
        subblocks: u32,
    },
    /// Everything, before anything.
    ///
    /// Also what a **declined** position reports, whatever the decline: no delivery is
    /// possible there and every pixel call errors anyway.
    WholeImage,
}

/// The error class a declined position would raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeclineClass {
    /// The position contradicts itself or the format.
    Malformed,
    /// The position is valid and self-consistent but uses something this version declines.
    Unsupported,
    /// The position tripped a configured cap.
    LimitExceeded,
    /// A data block failed verification.
    ChecksumMismatch,
}

/// Why the reader will not decode the position it is sitting on.
///
/// Without this a batch consumer would have to size a buffer and call `read_image_into`
/// speculatively just to learn a frame is undecodable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclineReason {
    pub(crate) class: DeclineClass,
    /// Shared rather than owned. Every message here interpolates the offending attribute
    /// verbatim — `sampleFormat {other:?}: …` — so a value at the `Attribute value length` cap
    /// makes a one-mebibyte message, and a declined `<Image>` reached by 256 root-level
    /// `<Reference>` elements copied it once per occurrence: 278 MB retained from a
    /// one-megabyte header.
    ///
    /// `Error` keeps its `String`s. Sharing here rather than widening the public error enum is
    /// the whole point: [`DeclineReason::to_error`] runs when a pixel call is actually made on
    /// a declined position, which is once, whereas the reason is cloned once per occurrence.
    /// The single `String` the caller sees is allocated there.
    pub(crate) reason: Arc<str>,
}

impl DeclineReason {
    // With neither format compiled in no position is ever reached, let alone declined, so
    // every constructor in the crate is unreachable in that configuration. § Operations makes
    // it a supported build, and a supported build must not be a noisy one.
    #[cfg_attr(not(any(feature = "fits", feature = "xisf")), allow(dead_code))]
    pub(crate) fn new(class: DeclineClass, reason: impl Into<Arc<str>>) -> Self {
        DeclineReason {
            class,
            reason: reason.into(),
        }
    }

    /// The class any pixel call on this position will raise.
    pub fn class(&self) -> DeclineClass {
        self.class
    }

    /// What was expected and what was found.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub(crate) fn to_error(&self) -> Error {
        Error::from(self)
    }
}

/// The same rendering [`Error`] gives the class this decline would raise, so a log line reads
/// alike whether the position was reported or decoded.
impl std::fmt::Display for DeclineReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let class = match self.class {
            DeclineClass::Malformed => "malformed",
            DeclineClass::Unsupported => "unsupported",
            DeclineClass::LimitExceeded => "limit exceeded",
            DeclineClass::ChecksumMismatch => "checksum mismatch",
        };
        write!(f, "{class}: {}", self.reason)
    }
}

/// The geometry three, reported as a unit — which is the invariant the three
/// [`Header`] accessors state in prose and this type states in the type system.
///
/// [`Header::geometry`] hands one back; `width`, `height` and `channels` stay for a caller
/// wanting one axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Geometry {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Channel count — `1` after `select_channel`.
    pub channels: u32,
}

impl Geometry {
    /// The file's total sample count, saturating at `u64::MAX`.
    ///
    /// **`u64` is not wide enough for the product, and saturating is the point.** Three `u32`
    /// axes reach `2⁹⁶`; a header declaring `4294967295` three times is about 400 bytes of
    /// XISF, so the hostile case is cheap to write. Multiplying plainly panics wherever
    /// overflow checks are on — every debug and test build, and `cargo-fuzz`, which § Fuzzing
    /// makes a release blocker — and wraps everywhere else, which is worse: a product
    /// congruent to something small modulo `2⁶⁴` passes the total-samples cap that exists to
    /// reject exactly this declaration.
    ///
    /// Saturating does not by itself bound the answer: `Limits::total_samples` is a public
    /// `u64`, and this repo's own tests set it to exactly `u64::MAX`, so the cap comparison
    /// can pass at that setting. What actually stops a saturated `u64::MAX` total from
    /// reaching an unchecked product downstream is the byte-extent gate right after the cap
    /// in `check_total_samples` (`total.checked_mul(width)`). Both run in the **pixel**
    /// phase, not the header phase — § Errors → Validation order names the total-samples
    /// check as one of the two checks deliberately outside the header-phase order — which is
    /// why it is `current_header_for_pixels` and `advance_chunk` that call it, not any
    /// header-phase parser.
    pub(crate) fn total_samples(&self) -> u64 {
        u64::from(self.width)
            .saturating_mul(u64::from(self.height))
            .saturating_mul(u64::from(self.channels))
    }
}

/// Everything parsed before pixels.
///
/// The organizing rule — report what the file says, interpret nothing — only means something
/// if the reported facts are reachable, and most of the XISF ones are XML *attributes*,
/// neither `FITSKeyword` nor `Property` elements, so no keyword lookup reaches them. `Header`
/// exposes them as typed accessors, format-independent where the two formats agree.
///
/// Returned by value rather than by borrow: a borrow would hold the `Reader` immutably while
/// every pixel-phase method needs `&mut self`, so the obvious usage — read the geometry, size
/// a buffer, decode into it — would not compile. The geometry and enums are trivially
/// copyable; the metadata collections are `Arc`-shared rather than deep-copied, and `Arc`
/// specifically rather than `Rc` because `Header` and `Image` must stay `Send` for a batch
/// consumer moving decoded frames between threads.
#[derive(Clone, Debug)]
pub struct Header {
    pub(crate) format: Format,
    pub(crate) geometry: Option<Geometry>,
    pub(crate) sample_format: Option<SampleFormat>,
    pub(crate) bounds: Bounds,
    pub(crate) scaling: Option<Scaling>,
    pub(crate) row_order: Option<RowOrder>,
    pub(crate) orientation: Option<Orientation>,
    pub(crate) offset: Option<f64>,
    pub(crate) color_space: Option<ColorSpace>,
    pub(crate) pixel_storage: Option<PixelStorage>,
    // `Arc<str>`, not `String`. One `<Image uid>` reached by N root-level `<Reference>`
    // elements is N occurrences of the same element, and each carries these attributes: a
    // 1 MiB `id` — exactly the `Attribute value length` cap — across 256 references retained
    // 262 MB from a one-megabyte header. The accessors return `&str` either way, so the
    // sharing is invisible from outside; see `Keyword`'s buffer for the same reasoning.
    pub(crate) image_id: Option<Arc<str>>,
    pub(crate) image_uuid: Option<Arc<str>>,
    pub(crate) image_type: Option<ImageType>,
    pub(crate) channel_index: Option<u32>,
    pub(crate) granularity: Granularity,
    pub(crate) decline_reason: Option<DeclineReason>,
    pub(crate) keywords: KeywordSet,
    pub(crate) properties: PropertySet,
    pub(crate) cfa: Option<Cfa>,
    pub(crate) resolution: Option<Resolution>,
    pub(crate) display_function: Option<DisplayFunction>,
}

impl Header {
    /// Which container this position was read from.
    ///
    /// A fact rather than an interpretation, and the one fact every format-specific accessor
    /// here was previously the only route to.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Image width in pixels.
    ///
    /// This and the next two are reported **as a unit** — all `Some` or all `None`, since a
    /// partial geometry is not a state this crate produces. All three are `Some` at every
    /// position this version will decode, that is, wherever [`Header::decline_reason`] is
    /// `None`.
    ///
    /// The rule for `None` is **representability, not validity**: a geometry this crate can
    /// read is reported even when what it reads is what declines the position. So plenty of
    /// declined positions report full geometry — a `BITPIX` outside the standard set, a
    /// tile-compressed `BINTABLE` reporting what its `Z*` keywords declare, and both
    /// spellings of an empty axis.
    ///
    /// `None` appears at FITS `NAXIS = 1` and `NAXIS > 3`; at an XISF `geometry` that cannot
    /// be read as a width, a height and a channel count — a wrong field count, a non-numeric
    /// field, a negative field, or a field beyond the representable width; and at any
    /// position whose geometry could not be parsed at all. The axis lengths a declined FITS
    /// position does declare stay reachable through [`Header::keywords`], which reports the
    /// structural cards like any others.
    pub fn width(&self) -> Option<u32> {
        self.geometry.map(|g| g.width)
    }

    /// Image height in pixels. See [`Header::width`].
    pub fn height(&self) -> Option<u32> {
        self.geometry.map(|g| g.height)
    }

    /// Channel count.
    ///
    /// After `select_channel` this reports `Some(1)`: the header describes what the reader
    /// will *produce*, not what the file happens to hold, so a caller sizing a buffer from
    /// the header is right by construction — **provided it fetches the header after
    /// configuring the reader**. See [`Header::width`] for when this is `None`.
    pub fn channels(&self) -> Option<u32> {
        self.geometry.map(|g| g.channels)
    }

    /// The three axes as one value, which is the all-or-nothing invariant
    /// [`Header::width`] states in prose said in the type system instead.
    ///
    /// `None` in exactly the cases the three accessors report `None`, and `Some` in exactly
    /// the cases they report `Some` — a caller sizing a destination reads one `Option` rather
    /// than reassembling three.
    pub fn geometry(&self) -> Option<Geometry> {
        self.geometry
    }

    /// The stored sample width.
    ///
    /// `None` for a `BITPIX` that is missing, unparseable or outside the standard set, and
    /// for a complex XISF sample format — the two facts with no representable form in this
    /// crate's model.
    pub fn sample_format(&self) -> Option<SampleFormat> {
        self.sample_format
    }

    /// The representable range actually in force, and where it came from.
    pub fn bounds(&self) -> &Bounds {
        &self.bounds
    }

    /// The FITS scaling in force, or `None` for XISF, which defines no such concept.
    ///
    /// FITS **always** reports [`Scaling::Fits`], materializing the `BSCALE = 1`,
    /// `BZERO = 0` defaults when the keywords are absent. Under `INHERIT` this reports the
    /// pair actually *applied*, so a mixture — an extension's own `BSCALE` beside the
    /// primary's `BZERO` — is visible rather than inferred.
    pub fn scaling(&self) -> Option<Scaling> {
        self.scaling
    }

    /// FITS `ROWORDER`, or `None` for XISF, which does not have the concept.
    pub fn row_order(&self) -> Option<&RowOrder> {
        self.row_order.as_ref()
    }

    /// XISF `orientation`, or `None` for FITS.
    pub fn orientation(&self) -> Option<&Orientation> {
        self.orientation.as_ref()
    }

    /// XISF `offset` — §11.5.2 calls it "also known as *pedestal*". Reported; subtracted from
    /// nothing.
    ///
    /// `Some` of §11.5.2's `0` default when the attribute is absent, and `None` on a FITS
    /// frame: the default is XISF's, and reporting it for a format that never stated one is
    /// indistinguishable to a caller from a file that did. The FITS `PEDESTAL` analogue stays
    /// a keyword, being a keyword — `keyword("PEDESTAL")` is where it is read.
    pub fn offset(&self) -> Option<f64> {
        self.offset
    }

    /// XISF `colorSpace`, or `None` for FITS, which declares no colour space.
    pub fn color_space(&self) -> Option<ColorSpace> {
        self.color_space
    }

    /// How the channels are laid out.
    ///
    /// XISF reports its `pixelStorage` attribute. FITS reports [`PixelStorage::Planar`]:
    /// that is not a fabricated attribute but the layout FITS 4.0 defines for a data array,
    /// where a `NAXIS = 3` cube is stored plane by plane.
    pub fn pixel_storage(&self) -> Option<PixelStorage> {
        self.pixel_storage
    }

    /// XISF `id`. Without this a caller stepping through a multi-image file cannot tell which
    /// image it holds.
    pub fn image_id(&self) -> Option<&str> {
        self.image_id.as_deref()
    }

    /// XISF `uuid`.
    pub fn image_uuid(&self) -> Option<&str> {
        self.image_uuid.as_deref()
    }

    /// XISF `imageType`.
    pub fn image_type(&self) -> Option<&ImageType> {
        self.image_type.as_ref()
    }

    /// Which channel **of the file** `select_channel` narrowed the reader to.
    ///
    /// `None` until `select_channel` is called, rather than `Some(0)`, so "no selection" and
    /// "selected channel zero" stay distinguishable. A chunk reports this same file index
    /// rather than renumbering to zero, while `Image::channel(k)` indexes the *image's* own
    /// channels. The two numbering schemes are deliberate and neither is derivable from the
    /// other.
    pub fn channel_index(&self) -> Option<u32> {
        self.channel_index
    }

    /// How much of the input the decoder must hold before it can produce any sample.
    pub fn granularity(&self) -> Granularity {
        self.granularity
    }

    /// `None` for an image this version will decode; `Some` on a declined position, carrying
    /// the class and the reason.
    pub fn decline_reason(&self) -> Option<&DeclineReason> {
        self.decline_reason.as_ref()
    }

    /// Every keyword this image carries, in document order, duplicates included.
    ///
    /// For a FITS image extension this is the extension's own cards followed by the primary
    /// header's, each tagged by [`Keyword::origin`]. Reporting is **never** gated on
    /// `INHERIT`: real archive frames put `DATE-OBS`, `EXPTIME` and `EGAIN` in the primary
    /// and frequently omit the keyword.
    ///
    /// For an XISF image it is exactly the `FITSKeyword` elements *that image* supplies — its
    /// own children plus root-level ones reaching it through a `Reference`. This crate never
    /// synthesizes a keyword that was not in the file.
    ///
    /// A [`Keywords`] view rather than a slice, for the reason [`Header::properties`] is one:
    /// the report is the same list in the same order, and what differs is that it is never
    /// materialized, because a merged list per image extension is a copy of the whole primary
    /// header per extension and nothing bounds that against the input. `KeywordSet` carries the
    /// arithmetic.
    pub fn keywords(&self) -> Keywords<'_> {
        self.keywords.view()
    }

    /// Look a keyword up by **exact** byte match, returning the **first** in stored order.
    ///
    /// Named for the surface it searches, as [`Header::property`] is: the two text surfaces
    /// are separate reports, and a lookup spelled `get` would leave a reader to guess which
    /// of them it reaches.
    ///
    /// Deliberately not case-folding: `keyword("SITELAT")` and `keyword("sitelat")` do not
    /// both resolve. A case-folding convenience is a consumer's adapter, not this crate's
    /// lookup.
    ///
    /// First-in-stored-order means an extension's own card wins over one inherited from the
    /// primary header — the live case, since both headers' cards are always reported and an
    /// inherited `EXPTIME` or `DATE-OBS` sits behind the extension's. Every occurrence stays
    /// reachable through [`Header::keywords`], which is what `HISTORY` and `COMMENT` need.
    pub fn keyword(&self, name: &str) -> Option<&Keyword> {
        self.keywords.view().iter().find(|k| k.name() == name)
    }

    /// The XISF properties that apply to the selected image, in document order, each tagged
    /// with the scope it came from.
    ///
    /// That image's own child properties, root-level `Metadata`-scope properties, and
    /// root-level ones reaching it by `Reference` — which take the position of the
    /// `Reference` element. Another image's child properties are **not** included, and
    /// neither is an unreferenced root-level property: it is attached to no image, and
    /// reporting it against an arbitrary one would invent an association the file does not
    /// make.
    ///
    /// Empty for FITS.
    ///
    /// **Duplicates are reported, exactly as [`Header::keywords`] reports duplicate cards.**
    /// Nothing in §11.1 makes an identifier unique within an image, and a root-level
    /// `<Property>` reached through several `<Reference>` elements is reported once per
    /// occurrence besides. So a repeated identifier is a list rather than a collision, and
    /// [`Header::property`] answering with the first is a documented selection rather than an
    /// assumption that there is only one.
    ///
    /// A [`Properties`] view rather than a slice. The report is the same list in the same
    /// order; what differs is that it is never materialized, because a merged list per image
    /// is a copy of the whole root `<Metadata>` list per image and nothing bounds that against
    /// the input. `PropertySet` carries the arithmetic.
    pub fn properties(&self) -> Properties<'_> {
        self.properties.view()
    }

    /// Look a property up by **exact** identifier match, returning the **first** in document
    /// order — the rule [`Header::keyword`] follows for keywords.
    ///
    /// An identifier is a case-sensitive token in §11.1.1, so this does not case-fold either.
    /// Every occurrence of a repeated identifier stays reachable through
    /// [`Header::properties`].
    pub fn property(&self, id: &str) -> Option<&Property> {
        self.properties.view().iter().find(|p| p.id() == id)
    }

    /// The XISF `ColorFilterArray`, if the image carries one.
    ///
    /// `None` for two different reasons, and the distinction matters to a caller: on a FITS
    /// frame because the format defines no such concept, and on an XISF image because absence
    /// of the element means the image is not mosaiced. [`Header::format`] is which of the two
    /// this `None` is. Unlike the other two mosaic-and-display accessors, `cfa()` has no
    /// specification default to report in the second case.
    ///
    /// That makes `None` on an XISF image a positive claim about the file — a consumer
    /// branching on it decides whether to debayer — so an element this crate cannot read is
    /// never reported that way. A `<ColorFilterArray>` the file does carry whose §11.10.1
    /// mandatory attributes are not all readable declines the position `Malformed`
    /// (§ Errors → Validation order) instead of reporting the image as un-mosaiced.
    ///
    /// **First wins.** §11.10 associates one CFA with one image, and an image carrying several
    /// `<ColorFilterArray>` children — its own, or ones reached through `<Reference>` — is
    /// reported from the first in document order, the rest ignored. Selection among
    /// well-formed elements states nothing the file does not, so it is not a decline.
    pub fn cfa(&self) -> Option<&Cfa> {
        self.cfa.as_ref()
    }

    /// The XISF `Resolution` — the specification's 72.0 ppi default when the element is absent
    /// (§11.11), and `None` on a FITS frame.
    ///
    /// FITS defines no resolution, and reporting 72.0 ppi for it would be an XISF number
    /// attributed to a format that never stated one. That is the fabricated value § The API
    /// rules out; the default belongs to XISF and is reported only there.
    ///
    /// The default answers an **absent** element only. A `<Resolution>` the file does carry
    /// whose `horizontal` or `vertical` is unreadable — unparseable, or the zero-or-negative
    /// §11.11.1 forbids — declines the position `Malformed`, because the alternative is 72.0
    /// reported beside a figure the file did state, with both attributed to the file.
    ///
    /// **First wins**, exactly as [`Header::cfa`] describes it.
    pub fn resolution(&self) -> Option<&Resolution> {
        self.resolution.as_ref()
    }

    /// The XISF `DisplayFunction` — the identity display function when the element is absent
    /// (§11.9), and `None` on a FITS frame, for the same reason [`Header::resolution`] is.
    /// Reported; applied to nothing.
    ///
    /// A present element whose §11.9.1 mandatory attributes are not all readable declines the
    /// position, and the first of several wins — both exactly as [`Header::resolution`]
    /// describes them.
    pub fn display_function(&self) -> Option<DisplayFunction> {
        self.display_function
    }
}
