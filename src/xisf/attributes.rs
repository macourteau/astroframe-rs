//! The `<Image>` attributes of §11.5, each read into the value it reports and the fault it
//! declines on.
//!
//! Every reader here is a pure function of one attribute's text. Representability and validity
//! are separate questions, so an attribute this crate can read is reported even when what it
//! reads is what declines the position, and each reader returns both halves; the order the
//! faults are taken in belongs to the caller. Nothing here sees the document or the walk's memo.

use crate::header::{Bounds, BoundsUnavailable, ColorSpace, DeclineReason, Geometry, PixelStorage};
use crate::normalize::SampleRange;
use crate::samples::SampleFormat;
use crate::xisf::block::{Checksum, parse_checksum};
use crate::xisf::cache::{decline_from, malformed, unsupported};
use crate::xisf::scalars::{parse_float, parse_u32, split_fields};

/// Read `geometry`, reporting on **representability** and declining on validity.
///
/// The two are different questions: a geometry this crate can read is reported even when what
/// it reads is what declines the position, which is why a zero-length axis reports full
/// geometry and is `Malformed` all the same (§8.5.1).
pub(super) fn read_geometry(attr: Option<&str>) -> (Option<Geometry>, Option<DeclineReason>) {
    let Some(text) = attr else {
        return (
            None,
            Some(malformed(
                "geometry: §11.5.1 makes the attribute mandatory for every Image element",
            )),
        );
    };
    let fields = split_fields(text);
    if fields.len() < 2 {
        return (
            None,
            Some(malformed(format!(
                "geometry {text:?}: §11.5.1 spells it dim_1:...:dim_N:channel-count, which \
                 needs at least two fields"
            ))),
        );
    }
    // Structural validity of the values is settled before scope, so a two-field geometry whose
    // fields are not integers is `Malformed` rather than a declined one-dimensional image.
    let Some(values) = fields
        .iter()
        .map(|f| parse_u32(f))
        .collect::<Option<Vec<u32>>>()
    else {
        return (
            None,
            Some(malformed(format!(
                "geometry {text:?}: every field is a §8.3.1 unsigned integer, and one of these \
                 is not, is negative, or is beyond the axis length this crate represents"
            ))),
        );
    };
    let [width, height, channels] = values[..] else {
        return (
            None,
            Some(unsupported(format!(
                "geometry {text:?} declares a {}-dimensional image; this version reads \
                 dim_1:dim_2:channel-count",
                values.len() - 1
            ))),
        );
    };
    let geometry = Geometry {
        width,
        height,
        channels,
    };
    if width == 0 || height == 0 || channels == 0 {
        // §8.5.1 calls this an empty image and states that empty images cannot be serialized
        // in an XISF unit — a file-validity rule rather than a scope decision.
        return (
            Some(geometry),
            Some(malformed(format!(
                "geometry {text:?} names a zero-length axis, which §8.5.1 forbids serializing"
            ))),
        );
    }
    (Some(geometry), None)
}

/// Read `colorSpace`. Absent means `Gray`, **never** inferred from channel count — and the
/// channel count is never validated against it: channels beyond a colour space's nominal count
/// are alpha channels and decode as ordinary channels.
pub(super) fn read_color_space(attr: Option<&str>) -> (Option<ColorSpace>, Option<DeclineReason>) {
    match attr {
        None | Some("Gray") => (Some(ColorSpace::Gray), None),
        Some("RGB") => (Some(ColorSpace::Rgb), None),
        Some("CIELab") => (
            Some(ColorSpace::CieLab),
            Some(unsupported(
                "colorSpace CIELab: recognized, and declined by this version",
            )),
        ),
        Some(other) => (
            None,
            Some(malformed(format!(
                "colorSpace {other:?}: §11.5.2 Table 14 defines Gray, RGB and CIELab, and \
                 decoding depends on the answer"
            ))),
        ),
    }
}

/// Read `sampleFormat`. A complex format is a recognized name this version declines; an
/// unrecognized spelling is `Malformed`, decoding depending on that enumeration.
pub(super) fn read_sample_format(
    attr: Option<&str>,
) -> (Option<SampleFormat>, Option<DeclineReason>) {
    match attr {
        None => (
            None,
            Some(malformed(
                "sampleFormat: §11.5.1 makes the attribute mandatory for every Image element",
            )),
        ),
        Some("UInt8") => (Some(SampleFormat::U8), None),
        Some("UInt16") => (Some(SampleFormat::U16), None),
        Some("UInt32") => (Some(SampleFormat::U32), None),
        Some("UInt64") => (Some(SampleFormat::U64), None),
        Some("Float32") => (Some(SampleFormat::F32), None),
        Some("Float64") => (Some(SampleFormat::F64), None),
        Some(complex @ ("Complex32" | "Complex64")) => (
            None,
            Some(unsupported(format!(
                "sampleFormat {complex}: a complex sample format has no representable form in \
                 this crate's output"
            ))),
        ),
        Some(other) => (
            None,
            Some(malformed(format!(
                "sampleFormat {other:?}: §11.5.1 Table 11 closes this enumeration, and decoding \
                 depends on it"
            ))),
        ),
    }
}

/// Read `byteOrder` (§10.4). Absent means little-endian — a wrong guess corrupts every sample
/// rather than producing a visible error.
pub(super) fn read_byte_order(attr: Option<&str>) -> (bool, Option<DeclineReason>) {
    match attr {
        None | Some("little") => (false, None),
        Some("big") => (true, None),
        Some(other) => (
            false,
            Some(malformed(format!(
                "byteOrder {other:?}: §10.4 defines big and little"
            ))),
        ),
    }
}

/// Read `pixelStorage`. Absent means `Planar`, never inferred from channel count.
pub(super) fn read_pixel_storage(
    attr: Option<&str>,
) -> (Option<PixelStorage>, Option<DeclineReason>) {
    match attr {
        None | Some("Planar") => (Some(PixelStorage::Planar), None),
        Some("Normal") => (Some(PixelStorage::Normal), None),
        Some(other) => (
            None,
            Some(malformed(format!(
                "pixelStorage {other:?}: §11.5.2 Table 13 defines Planar and Normal, and \
                 decoding depends on the answer"
            ))),
        ),
    }
}

/// Read `offset` (§11.5.2), which reports its `0` default when the attribute is absent.
pub(super) fn read_offset(attr: Option<&str>) -> (f64, Option<DeclineReason>) {
    let Some(text) = attr else {
        return (0.0, None);
    };
    let Some(value) = parse_float(text) else {
        return (
            0.0,
            Some(malformed(format!(
                "offset {text:?} is not a §8.3.3 floating point scalar"
            ))),
        );
    };
    // §11.5.2 puts the constraint here rather than in §8.3.1's grammar, and `NaN`/`-Inf`
    // spellings are expressible via §8.3.3, so both are named rather than left to a
    // comparison that a NaN would pass.
    if value.is_nan() || value < 0.0 {
        return (
            value,
            Some(malformed(format!(
                "offset {text:?}: §11.5.2 requires a value greater than or equal to zero"
            ))),
        );
    }
    (value, None)
}

pub(super) fn read_checksum(attr: Option<&str>) -> (Option<Checksum>, Option<DeclineReason>) {
    match attr.map(parse_checksum) {
        None => (None, None),
        Some(Ok(checksum)) => (Some(checksum), None),
        Some(Err(e)) => (None, Some(decline_from(e))),
    }
}

// ------------------------------------------------------------------ the representable range

/// Where the representable range comes from for an XISF image.
///
/// Evaluated at parse and raised at normalize, so an invalid declaration is not a decline: the
/// header parses, this reports `Unavailable(InvalidDeclared)`, native samples decode, and only
/// the normalizing call refuses.
pub(super) fn read_bounds(declared: Option<&str>, format: Option<SampleFormat>) -> Bounds {
    if let Some(text) = declared {
        let fields = split_fields(text);
        if let [lo, hi] = fields[..]
            && let (Some(lo), Some(hi)) = (parse_float(lo), parse_float(hi))
            // The validity rule is stated on `k` rather than on the endpoints, and it applies
            // to integer images too. The checked range is what `Bounds` carries, so nothing
            // downstream re-derives it.
            && let Some(range) = SampleRange::new(lo, hi)
        {
            return Bounds::Declared(range);
        }
        return Bounds::Unavailable(BoundsUnavailable::InvalidDeclared);
    }
    match format {
        // §8.5.5's [0, 2ⁿ − 1] default, which §11.5.1 makes `bounds` optional against.
        Some(f) if f.is_integer() => match SampleRange::unsigned_default(f.bytes() * 8) {
            Some(range) => Bounds::FormatDefault(range),
            None => Bounds::Unavailable(BoundsUnavailable::NoFormatDefault),
        },
        // §11.5.1 makes `bounds` mandatory for a floating point real image, and a missing one
        // is scoped exactly like an unparseable one.
        Some(_) => Bounds::Unavailable(BoundsUnavailable::InvalidDeclared),
        // No sample format this crate represents, so there is no default to report.
        None => Bounds::Unavailable(BoundsUnavailable::NoFormatDefault),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{DeclineClass, Orientation};
    use crate::metadata::{DisplayFunction, Resolution};
    use crate::xisf::image::tests::{class, image, one, tiny};

    // -------------------------------------------------------------- colour and channels

    #[test]
    fn the_three_legal_colour_space_and_channel_combinations_decode() {
        // Channel count is never validated against the colour space: channels beyond its
        // nominal count are alpha channels and decode as ordinary channels.
        let absent_three = one(
            r#"<xisf version="1.0"><Image geometry="4:4:3" sampleFormat="UInt8"
                 location="attachment:1024:48"/></xisf>"#,
        );
        assert_eq!(class(&absent_three), None);
        assert_eq!(absent_three.header.color_space(), Some(ColorSpace::Gray));
        assert_eq!(absent_three.header.channels(), Some(3));

        let gray_two = one(
            r#"<xisf version="1.0"><Image geometry="4:4:2" sampleFormat="UInt8"
                 colorSpace="Gray" location="attachment:1024:32"/></xisf>"#,
        );
        assert_eq!(class(&gray_two), None);

        let rgb_four = one(
            r#"<xisf version="1.0"><Image geometry="4:4:4" sampleFormat="UInt8"
                 colorSpace="RGB" location="attachment:1024:64"/></xisf>"#,
        );
        assert_eq!(class(&rgb_four), None);
        assert_eq!(rgb_four.header.color_space(), Some(ColorSpace::Rgb));
    }

    #[test]
    fn the_defaults_are_the_specifications_and_are_never_inferred() {
        let o = one(&tiny(r#"location="attachment:1024:16""#));
        assert_eq!(o.header.pixel_storage(), Some(PixelStorage::Planar));
        assert_eq!(o.header.color_space(), Some(ColorSpace::Gray));
        assert_eq!(o.header.orientation(), Some(&Orientation::Identity));
        assert_eq!(o.header.row_order(), None);
        assert_eq!(o.header.scaling(), None);
        assert_eq!(o.header.offset(), Some(0.0));
        assert_eq!(o.header.resolution(), Some(&Resolution::default()));
        assert_eq!(
            o.header.display_function(),
            Some(DisplayFunction::default())
        );
        assert!(o.header.cfa().is_none());
    }

    // -------------------------------------------------------------- validation order

    #[test]
    fn an_unsupported_attribute_beats_a_missing_location() {
        // The location check runs last, which is the whole reason these fixtures are
        // `Unsupported` rather than `Malformed`.
        let o =
            one(r#"<xisf version="1.0"><Image geometry="4:4:1" sampleFormat="Complex32"/></xisf>"#);
        assert_eq!(class(&o), Some(DeclineClass::Unsupported));
        assert_eq!(o.header.sample_format(), None);
        assert_eq!(o.header.width(), Some(4));

        let cielab = one(
            r#"<xisf version="1.0"><Image geometry="4:4:1" sampleFormat="UInt8"
                 colorSpace="CIELab"/></xisf>"#,
        );
        assert_eq!(class(&cielab), Some(DeclineClass::Unsupported));
    }

    #[test]
    fn an_unrecognized_enumeration_value_is_malformed_and_so_is_a_missing_location() {
        let unknown = one(&image(
            r#"geometry="4:4:1" sampleFormat="UInt24" location="attachment:1024:16""#,
        ));
        assert_eq!(class(&unknown), Some(DeclineClass::Malformed));
        assert_eq!(unknown.header.sample_format(), None);

        let storage = one(&tiny(
            r#"pixelStorage="Chunky" location="attachment:1024:16""#,
        ));
        assert_eq!(class(&storage), Some(DeclineClass::Malformed));

        let order = one(&tiny(r#"byteOrder="middle" location="attachment:1024:16""#));
        assert_eq!(class(&order), Some(DeclineClass::Malformed));

        let no_location =
            one(r#"<xisf version="1.0"><Image geometry="4:4:1" sampleFormat="UInt8"/></xisf>"#);
        assert_eq!(class(&no_location), Some(DeclineClass::Malformed));
        assert!(no_location.plan.is_none());
    }

    #[test]
    fn geometry_reports_on_representability_and_declines_on_validity() {
        // A zero-length axis still *reads*, so it reports full geometry.
        let empty = one(&image(
            r#"geometry="4:0:1" sampleFormat="UInt8" location="attachment:1024:16""#,
        ));
        assert_eq!(class(&empty), Some(DeclineClass::Malformed));
        assert_eq!(empty.header.width(), Some(4));
        assert_eq!(empty.header.height(), Some(0));
        assert_eq!(empty.header.channels(), Some(1));

        // A valid one-dimensional image is declined, and reports no geometry.
        let one_d = one(&image(
            r#"geometry="4:1" sampleFormat="UInt8" location="attachment:1024:16""#,
        ));
        assert_eq!(class(&one_d), Some(DeclineClass::Unsupported));
        assert_eq!(one_d.header.width(), None);

        // Four dimensions likewise.
        let four_d = one(&image(
            r#"geometry="4:4:4:1" sampleFormat="UInt8" location="attachment:1024:16""#,
        ));
        assert_eq!(class(&four_d), Some(DeclineClass::Unsupported));

        // One field is not a geometry at all.
        let single = one(&image(
            r#"geometry="4" sampleFormat="UInt8" location="attachment:1024:16""#,
        ));
        assert_eq!(class(&single), Some(DeclineClass::Malformed));

        // A negative field has no unsigned value to report.
        let negative = one(&image(
            r#"geometry="4:-4:1" sampleFormat="UInt8" location="attachment:1024:16""#,
        ));
        assert_eq!(class(&negative), Some(DeclineClass::Malformed));
        assert_eq!(negative.header.width(), None);
    }

    #[test]
    fn a_negative_offset_is_malformed_and_a_positive_one_is_reported() {
        let negative = one(&tiny(r#"location="attachment:1024:16" offset="-1""#));
        assert_eq!(class(&negative), Some(DeclineClass::Malformed));

        // `NaN` and `-Inf` are conforming §8.3.3 spellings, so they reach the range check.
        let nan = one(&tiny(r#"location="attachment:1024:16" offset="NaN""#));
        assert_eq!(class(&nan), Some(DeclineClass::Malformed));

        let pedestal = one(&tiny(r#"location="attachment:1024:16" offset="128.5""#));
        assert_eq!(class(&pedestal), None);
        assert_eq!(pedestal.header.offset(), Some(128.5));
    }

    // -------------------------------------------------------------- bounds

    #[test]
    fn an_integer_bounds_failing_the_validity_rule_still_parses_its_header() {
        // `k = 1/(1-1)` is not finite, so the range is unusable — and the rule applies to
        // integer images too.
        let o = one(&tiny(r#"location="attachment:1024:16" bounds="1:1""#));
        assert_eq!(class(&o), None, "an invalid bounds is not a decline");
        assert_eq!(
            *o.header.bounds(),
            Bounds::Unavailable(BoundsUnavailable::InvalidDeclared)
        );
        // The file's own text survives, usable or not.
        assert_eq!(o.declared_bounds.as_deref(), Some("1:1"));
        assert!(o.plan.is_some(), "native samples still decode");
    }

    #[test]
    fn an_integer_image_without_bounds_takes_the_format_default() {
        let o = one(&tiny(r#"location="attachment:1024:16""#));
        assert!(
            matches!(o.header.bounds(), Bounds::FormatDefault(r) if r.lo() == 0.0 && r.hi() == 255.0)
        );
        assert_eq!(o.declared_bounds, None);

        // Legal per §11.5.1, which only says such a bounds *should not* be written.
        let declared = one(&tiny(r#"location="attachment:1024:16" bounds="0:255""#));
        assert!(
            matches!(declared.header.bounds(), Bounds::Declared(r) if r.lo() == 0.0 && r.hi() == 255.0)
        );
    }

    #[test]
    fn a_float_image_without_bounds_is_unavailable_and_still_parses() {
        let o = one(
            r#"<xisf version="1.0"><Image geometry="4:4:1" sampleFormat="Float32"
                 location="attachment:1024:64"/></xisf>"#,
        );
        assert_eq!(class(&o), None);
        assert_eq!(
            *o.header.bounds(),
            Bounds::Unavailable(BoundsUnavailable::InvalidDeclared)
        );

        let declared = one(
            r#"<xisf version="1.0"><Image geometry="4:4:1" sampleFormat="Float32"
                 bounds="0:1" location="attachment:1024:64"/></xisf>"#,
        );
        assert!(
            matches!(declared.header.bounds(), Bounds::Declared(r) if r.lo() == 0.0 && r.hi() == 1.0)
        );
    }

    #[test]
    fn a_bounds_naming_the_wrong_field_count_is_unavailable() {
        let o = one(&tiny(r#"location="attachment:1024:16" bounds="0""#));
        assert_eq!(
            *o.header.bounds(),
            Bounds::Unavailable(BoundsUnavailable::InvalidDeclared)
        );
        assert_eq!(o.declared_bounds.as_deref(), Some("0"));
    }
}
