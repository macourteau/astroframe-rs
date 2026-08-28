//! The numeric-correctness criteria, graded against `src/normalize.rs` alone.
//!
//! These need no file: the primitive is a pure function of a raw sample, the scaling and
//! the range, which is why the design puts them before any container format exists.
//!
//! The figures asserted here are golden — reproduced five times across separate design
//! reviews. If one appears wrong, that is a finding worth raising rather than an edit
//! worth making.

use astroframe::{Normalizer, SampleRange, Scaling};

/// The pinned reference form for a 16-bit unsigned image, written out longhand.
fn reference_u16(level: u16) -> f32 {
    level as f32 * (1.0f32 / 65535.0f32)
}

/// The pinned reference form for an 8-bit unsigned image.
fn reference_u8(level: u8) -> f32 {
    level as f32 * (1.0f32 / 255.0f32)
}

fn unsigned(bits: u32) -> Normalizer {
    Normalizer::new(
        None,
        SampleRange::unsigned_default(bits).expect("standard width"),
    )
}

/// Criterion *Exhaustive `UInt16` normalization*.
#[test]
fn exhaustive_uint16_normalization() {
    let n = unsigned(16);
    for level in 0u16..=u16::MAX {
        let got = n.normalize(level);
        let want = reference_u16(level);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "level {level}: got {got:?} ({:#010x}), want {want:?} ({:#010x})",
            got.to_bits(),
            want.to_bits()
        );
    }
}

/// Criterion *Exhaustive `UInt8` normalization*.
#[test]
fn exhaustive_uint8_normalization() {
    let n = unsigned(8);
    for level in 0u8..=u8::MAX {
        let got = n.normalize(level);
        let want = reference_u8(level);
        assert_eq!(got.to_bits(), want.to_bits(), "level {level}");
    }
}

/// Criterion *The divergence is pinned, not merely avoided*.
///
/// If someone "simplifies" the primitive to a division, this says so in plain numbers.
#[test]
fn divergence_is_pinned_not_merely_avoided() {
    let n16 = unsigned(16);
    let mut differing16 = Vec::new();
    for level in 0u16..=u16::MAX {
        let multiply = n16.normalize(level);
        let divide = level as f32 / 65535.0f32;
        if multiply.to_bits() != divide.to_bits() {
            differing16.push(level);
            let ulps = (multiply.to_bits() as i64 - divide.to_bits() as i64).abs();
            assert_eq!(ulps, 1, "level {level} differs by more than 1 ULP");
        }
    }
    assert_eq!(
        differing16.len(),
        512,
        "the two forms must differ on exactly 512 of 65536 16-bit levels"
    );
    assert_eq!(
        &differing16[..6],
        &[257u16, 261, 265, 269, 273, 277],
        "first differing 16-bit levels"
    );
    // The one worked example the design records to full precision.
    assert_eq!(
        format!("{:.12e}", n16.normalize(257u16)),
        "3.921568393707e-3"
    );
    assert_eq!(format!("{:.12e}", 257f32 / 65535.0f32), "3.921568859369e-3");

    let n8 = unsigned(8);
    let mut differing8 = Vec::new();
    for level in 0u8..=u8::MAX {
        let multiply = n8.normalize(level);
        let divide = level as f32 / 255.0f32;
        if multiply.to_bits() != divide.to_bits() {
            differing8.push(level);
            let ulps = (multiply.to_bits() as i64 - divide.to_bits() as i64).abs();
            assert_eq!(ulps, 1, "level {level} differs by more than 1 ULP");
        }
    }
    assert_eq!(
        differing8.len(),
        126,
        "the two forms must differ on exactly 126 of 256 8-bit levels"
    );
    assert_eq!(
        &differing8[..6],
        &[3u8, 6, 7, 12, 13, 14],
        "first differing 8-bit levels"
    );
}

/// Criterion *Endpoints are exact at every integer width*.
#[test]
fn endpoints_are_exact_at_every_integer_width() {
    assert_eq!(unsigned(8).normalize(0u8).to_bits(), 0x0000_0000);
    assert_eq!(unsigned(8).normalize(u8::MAX).to_bits(), 1.0f32.to_bits());

    assert_eq!(unsigned(16).normalize(0u16).to_bits(), 0x0000_0000);
    assert_eq!(unsigned(16).normalize(u16::MAX).to_bits(), 1.0f32.to_bits());

    assert_eq!(unsigned(32).normalize(0u32).to_bits(), 0x0000_0000);
    assert_eq!(unsigned(32).normalize(u32::MAX).to_bits(), 1.0f32.to_bits());

    assert_eq!(unsigned(64).normalize(0u64).to_bits(), 0x0000_0000);
    assert_eq!(unsigned(64).normalize(u64::MAX).to_bits(), 1.0f32.to_bits());
}

/// The companion half of *Endpoints are exact*: the lossiness is pinned as intended
/// behaviour rather than discovered later as a bug.
///
/// It must sample **high** levels — collisions do not occur at the bottom of the range,
/// where `f32` spacing is far finer than the level spacing.
#[test]
fn wide_integer_levels_collide_in_normalized_output() {
    let n32 = unsigned(32);
    assert_eq!(
        n32.normalize(u32::MAX).to_bits(),
        n32.normalize(u32::MAX - 1).to_bits(),
        "distinct high UInt32 levels must collide"
    );
    // ...and the bottom of the range is exactly where they do not.
    assert_ne!(n32.normalize(0u32).to_bits(), n32.normalize(1u32).to_bits());

    let n64 = unsigned(64);
    assert_eq!(
        n64.normalize(u64::MAX).to_bits(),
        n64.normalize(u64::MAX - 1).to_bits(),
        "distinct high UInt64 levels must collide"
    );
    assert_ne!(n64.normalize(0u64).to_bits(), n64.normalize(1u64).to_bits());

    // UInt8 and UInt16 are lossless and reversible: no two levels collide anywhere.
    let n16 = unsigned(16);
    let mut seen = std::collections::HashSet::with_capacity(65536);
    for level in 0u16..=u16::MAX {
        assert!(
            seen.insert(n16.normalize(level).to_bits()),
            "UInt16 level {level} collides with an earlier level"
        );
    }
}

/// Criterion *Non-finite handling is total*.
#[test]
fn non_finite_handling_is_total() {
    let n = Normalizer::new(None, SampleRange::new(0.0, 1.0).expect("unit range"));

    assert_eq!(n.normalize(f32::INFINITY).to_bits(), 1.0f32.to_bits());
    assert_eq!(n.normalize(f32::NEG_INFINITY).to_bits(), 0x0000_0000);
    assert!(n.normalize(f32::NAN).is_nan());
    assert_eq!(n.normalize(f64::INFINITY).to_bits(), 1.0f32.to_bits());
    assert_eq!(n.normalize(f64::NEG_INFINITY).to_bits(), 0x0000_0000);
    assert!(n.normalize(f64::NAN).is_nan());

    // A sample below `lo` decodes to `+0.0` with the sign bit clear, never `-0.0`.
    assert_eq!(n.normalize(-0.5f32).to_bits(), 0x0000_0000);
    assert_eq!(n.normalize(-1.0e30f64).to_bits(), 0x0000_0000);

    // A stored `-0.0` with `lo == +0.0` reaches step 3 unclamped, `-0.0` not being *below*
    // `+0.0`. The zero normalization is what removes the sign bit.
    assert_eq!(n.normalize(-0.0f32).to_bits(), 0x0000_0000);
    assert_eq!(n.normalize(0.0f32).to_bits(), 0x0000_0000);

    // Above `hi` saturates, and the saturation is the lossy step the design accepts:
    // an identity-multiply `Float32` frame with `bounds="0:1"` flattens out-of-range
    // samples to the bounds and nothing else happens to it.
    assert_eq!(n.normalize(1.5f32).to_bits(), 1.0f32.to_bits());

    // NaN is preserved rather than folded to a bound, which is what a `min`/`max` clamp
    // would do. A shifted-range frame reaches the same answer.
    let shifted = Normalizer::new(None, SampleRange::new(-5.0, 5.0).expect("range"));
    assert!(shifted.normalize(f32::NAN).is_nan());
    assert_eq!(shifted.normalize(f32::NEG_INFINITY).to_bits(), 0x0000_0000);
    assert_eq!(shifted.normalize(f32::INFINITY).to_bits(), 1.0f32.to_bits());
    assert_eq!(shifted.normalize(-5.0f32).to_bits(), 0x0000_0000);
}

/// Criterion *Range validity is one rule about `k`* — the half reachable without a
/// container. The `set_bounds` half is graded in the reader tests.
#[test]
fn range_validity_is_one_rule_about_k() {
    // The cases an endpoint check also catches.
    assert!(SampleRange::new(1.0, 1.0).is_none(), "lo == hi");
    assert!(SampleRange::new(2.0, 1.0).is_none(), "lo > hi");
    assert!(SampleRange::new(f64::NAN, 1.0).is_none(), "non-finite lo");
    assert!(SampleRange::new(0.0, f64::NAN).is_none(), "non-finite hi");
    assert!(
        SampleRange::new(0.0, f64::INFINITY).is_none(),
        "infinite hi"
    );
    assert!(
        SampleRange::new(f64::NEG_INFINITY, 0.0).is_none(),
        "infinite lo"
    );

    // The cases an endpoint check misses. All three have finite ordered endpoints.
    assert!(
        SampleRange::new(0.0, 1e-46).is_none(),
        "width underflows to zero in f32; every in-range sample would decode to NaN"
    );
    assert!(
        SampleRange::new(0.0, 5e-45).is_none(),
        "width narrows to a subnormal; the frame would decode uniformly white"
    );
    assert!(
        SampleRange::new(-1e308, 1e308).is_none(),
        "width overflows in f64 before the cast; the frame would decode to NaN"
    );

    // The rule is stated on `k`, and the boundary is exactly where the design measured it.
    // 2^126 is exact in both f32 and f64; `powi` is banned on this path, so it is built by
    // shift rather than by exponentiation.
    let w126 = (1u128 << 126) as f64;
    let at_boundary =
        SampleRange::new(0.0, w126).expect("2^126 is the last width the rule accepts");
    assert_eq!(
        at_boundary.reciprocal().to_bits(),
        f32::MIN_POSITIVE.to_bits(),
        "k at width 2^126 is exactly f32::MIN_POSITIVE"
    );

    // The next representable f32 width above it gives the largest f32 subnormal, which the
    // rule rejects. A width test would have admitted that whole band.
    let next_width = f64::from(f32::from_bits((w126 as f32).to_bits() + 1));
    assert!(next_width > w126);
    assert!(
        SampleRange::new(0.0, next_width).is_none(),
        "a normal width whose reciprocal is subnormal is rejected"
    );
    assert!(
        SampleRange::new(0.0, 1e38).is_none(),
        "k = 1.000e-38, subnormal"
    );
    assert!(
        SampleRange::new(0.0, f64::from(f32::MAX)).is_none(),
        "k = 2.939e-39, subnormal"
    );

    // Knowingly asymmetric the other way: a subnormal *width* whose reciprocal is normal
    // passes, costing one to three mantissa bits.
    let subnormal_width = SampleRange::new(0.0, 1e-38).expect("subnormal width, normal reciprocal");
    assert!(!((subnormal_width.hi() - subnormal_width.lo()) as f32).is_normal());
    assert!(subnormal_width.reciprocal().is_normal());
}

/// The endpoint-exactness claim is scoped to default integer ranges, and this is why.
///
/// `((hi - lo) as f32) * k` depends only on the width's mantissa, so one binade settles
/// every declared range there is.
#[test]
fn declared_range_endpoints_are_not_generally_exact() {
    let one = 1.0f32.to_bits();
    let one_ulp_low = 0x3F7F_FFFFu32;

    let mut exact = 0u32;
    let mut low = 0u32;
    for mantissa in 0..(1u32 << 23) {
        let width = f32::from_bits(one + mantissa); // widths across [1, 2)
        let k = 1.0f32 / width;
        match (width * k).to_bits() {
            b if b == one => exact += 1,
            b if b == one_ulp_low => low += 1,
            other => panic!("width {width:?} produced {other:#010x}, neither 1.0 nor one ULP low"),
        }
    }

    let total = f64::from(1u32 << 23);
    assert_eq!(
        format!("{:.4}", f64::from(exact) / total * 100.0),
        "84.6533",
        "share of declared ranges whose high endpoint lands exactly on 1.0"
    );
    assert_eq!(
        format!("{:.4}", f64::from(low) / total * 100.0),
        "15.3467",
        "share landing one ULP low"
    );
    // None land high, so the saturating clamp cannot repair it.
    assert_eq!(exact + low, 1u32 << 23);
}

/// The generalized three-step form must agree with the reference two-step form on the FITS
/// unsigned convention, which is what makes the required form fall out without a special
/// case.
#[test]
fn fits_unsigned_convention_matches_the_reference_two_step_form() {
    // BITPIX = 16, BZERO = 32768, BSCALE = 1 — signed storage, unsigned physical values.
    let n = Normalizer::new(
        Some(Scaling::Fits {
            bscale: 1.0,
            bzero: 32768.0,
        }),
        SampleRange::unsigned_default(16).expect("16-bit default"),
    );

    for level in 0u16..=u16::MAX {
        let stored = (level as i32 - 32768) as i16;
        let got = n.normalize(stored);
        let want = reference_u16(level);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "level {level} (stored {stored})"
        );
    }

    assert_eq!(n.normalize(i16::MIN).to_bits(), 0x0000_0000);
    assert_eq!(n.normalize(i16::MAX).to_bits(), 1.0f32.to_bits());
}

/// The slice-wise method is exactly a loop over the per-sample one, which is what makes
/// autovectorization safe to permit.
#[test]
fn slice_normalization_equals_per_sample() {
    let n = unsigned(16);
    let src: Vec<u16> = (0u16..=u16::MAX).collect();
    let mut dst = vec![0.0f32; src.len()];
    n.normalize_into(&src, &mut dst);
    for (i, (&raw, &out)) in src.iter().zip(dst.iter()).enumerate() {
        assert_eq!(out.to_bits(), n.normalize(raw).to_bits(), "index {i}");
    }
}
