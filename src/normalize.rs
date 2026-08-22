//! Layer 2 — the normalization primitive.
//!
//! This module is deliberately alone: it holds the three-step sample-to-`[0,1]` primitive
//! and the range type, and it contains no `use` of any format module. That import ban is
//! the mechanical enforcement of invariant I1 — one normalization, shared by every
//! container — and it is checked in CI. It is also why nothing here touches I/O: the
//! exhaustive tests that pin the arithmetic need no file at all.
//!
//! # The form is pinned, and it is not the idiomatic one
//!
//! Every sample passes through three steps, in this order, with the casts exactly where
//! they are written:
//!
//! ```text
//! step 1  physical : f64 = bscale * (raw as f64) + bzero   // only with FITS scaling
//!         physical : f64 = raw as f64                      // otherwise
//! step 2  shifted  : f32 = (physical - lo) as f32
//! step 3  out      : f32 = shifted * k                     // k : f32 = 1.0 / ((hi - lo) as f32)
//! ```
//!
//! `k` is computed **once per image**, in `f32`, as the reciprocal of the range width.
//! The obvious simplification — `raw as f32 / 65535.0` — is a *different function*:
//! division is correctly rounded, one rounding, where this form has two. The two disagree
//! on 512 of 65 536 `UInt16` levels and on 126 of 256 `UInt8` levels. A difference of
//! exactly that size is enough to shift a computed frame median.
//!
//! Do not simplify this. See `docs/intentional-patterns.md` for the full argument, and
//! `tests/normalization.rs`, which fails loudly in plain numbers if anyone does.
//!
//! # Guarantee scope
//!
//! Bit-exactness is guaranteed on targets with IEEE-754 `f32`/`f64` arithmetic and no
//! excess precision — every `x86_64-*`, `aarch64-*`, `wasm32-*`, and any 32-bit x86 target
//! with SSE2. It does not cover x87-only targets such as `i586-*`. It also assumes the
//! default floating-point environment: round-to-nearest-even, subnormals preserved. A host
//! process that has set FTZ/DAZ in `MXCSR` breaks it from outside this crate.
//!
//! On `wasm32` a NaN result's payload and sign bit are nondeterministic by specification,
//! so NaN is compared with `is_nan()` there rather than `to_bits()`.

/// A native sample width a container can store.
///
/// Sealed: the set is exactly what FITS and XISF can hold, which is what makes a complex
/// sample format unrepresentable in this crate's output rather than merely unimplemented.
pub trait Sample: Copy + sealed::Sealed {
    /// Widen to `f64` for step 1.
    ///
    /// For `U64` and `I64` this is lossy above 2^53, which is inherent to doing the
    /// scaling in `f64` at all — the form the contract requires.
    fn widen(self) -> f64;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! impl_sample {
    ($($t:ty),* $(,)?) => {$(
        impl sealed::Sealed for $t {}
        impl Sample for $t {
            #[inline]
            fn widen(self) -> f64 {
                self as f64
            }
        }
    )*};
}

impl_sample!(u8, u16, u32, u64, i16, i32, i64, f32, f64);

/// The scaling a container applies before the range map.
///
/// `None` at the call sites that report it means the container defines no such concept —
/// which is to say XISF. FITS always reports [`Scaling::Fits`], materializing the
/// `BSCALE = 1`, `BZERO = 0` defaults when the keywords are absent.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Scaling {
    /// The FITS linear scaling, `physical = BSCALE * raw + BZERO`.
    Fits {
        /// `BSCALE`, defaulting to 1.0 when the keyword is absent.
        bscale: f64,
        /// `BZERO`, defaulting to 0.0 when the keyword is absent.
        bzero: f64,
    },
}

/// A validated representable range: the `lo` and `hi` physical values that map to 0.0 and 1.0.
///
/// Construction enforces the one validity rule, and that rule is about the *reciprocal*
/// rather than about the endpoints — see [`Range::new`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    lo: f64,
    hi: f64,
    k: f32,
}

impl Range {
    /// Build a range, or reject it.
    ///
    /// > A range is valid when `k = 1.0f32 / ((hi - lo) as f32)` is **finite, positive and
    /// > normal**.
    ///
    /// Checking the endpoints instead is the obvious formulation and is not sufficient:
    /// `(0, 1e-46)` has finite ordered endpoints and a width that narrows to `0.0`, giving
    /// `k = +Inf` and a frame that decodes entirely to NaN. `(0, 5e-45)` narrows to a
    /// subnormal and decodes uniformly white. `(-1e308, 1e308)` overflows in `f64` before
    /// the cast and decodes to `+0.0` at `lo` and NaN everywhere else. All three are
    /// reachable from a file-declared `bounds` (XISF §8.3.3 admits any float spelling) and
    /// from `with_bounds`, and all three look like a decode rather than a failure.
    ///
    /// The rule is knowingly asymmetric the other way: a subnormal *width* whose reciprocal
    /// is normal passes, costing one to three mantissa bits. Such a range is not a
    /// plausible image, and the rule's job is to catch the frames that come out uniformly
    /// black, white or NaN.
    ///
    /// Returns `None` rather than an error so the caller picks the class: a file-declared
    /// `bounds` failing this rule is `Malformed`, the same values from `with_bounds` are
    /// `InvalidRequest`.
    #[inline]
    pub fn new(lo: f64, hi: f64) -> Option<Self> {
        // The `as f32` on the width is load-bearing, not decorative. Narrowing first and
        // then taking the reciprocal is bit-identical to taking it in f64 and narrowing
        // the result; taking the reciprocal of the *un-narrowed* f64 width disagrees on
        // roughly a quarter of widths.
        let k = 1.0f32 / ((hi - lo) as f32);
        if k.is_finite() && k > 0.0 && k.is_normal() {
            Some(Range { lo, hi, k })
        } else {
            None
        }
    }

    /// The default representable range for an `n`-bit unsigned integer image: `0`, `2ⁿ − 1`.
    ///
    /// `bits` is 8, 16, 32 or 64; anything else returns `None`.
    pub fn unsigned_default(bits: u32) -> Option<Self> {
        let hi = match bits {
            8 => f64::from(u8::MAX),
            16 => f64::from(u16::MAX),
            32 => f64::from(u32::MAX),
            64 => u64::MAX as f64,
            _ => return None,
        };
        Range::new(0.0, hi)
    }

    /// The low endpoint — the physical value that maps to `0.0`.
    #[inline]
    pub fn lo(&self) -> f64 {
        self.lo
    }

    /// The high endpoint — the physical value that maps to `1.0`, exactly for the default
    /// integer ranges and to within one ULP for an arbitrary declared range.
    #[inline]
    pub fn hi(&self) -> f64 {
        self.hi
    }

    /// `k`, the `f32` reciprocal of the range width, computed once here rather than per
    /// sample.
    #[inline]
    pub fn reciprocal(&self) -> f32 {
        self.k
    }
}

/// The normalization primitive: everything between a raw stored sample and an `f32` in
/// `[0,1]`.
///
/// Built once per image from the scaling and the range, so `k` is computed once rather
/// than per sample. It owns every step — the `f64` scaling, the narrowing cast, the
/// multiply, the saturating clamp and the zero normalization — because a caller handed an
/// `f64` would already have performed step 1, and the streaming-equals-whole-buffer
/// guarantee would then be pinning the caller's arithmetic rather than this crate's.
///
/// It is reachable independently of any container format, which is what lets a build with
/// both format features off still be useful.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Normalizer {
    scaling: Option<Scaling>,
    range: Range,
}

impl Normalizer {
    /// Build the primitive for one image.
    pub fn new(scaling: Option<Scaling>, range: Range) -> Self {
        Normalizer { scaling, range }
    }

    /// The range in force.
    #[inline]
    pub fn range(&self) -> Range {
        self.range
    }

    /// The scaling in force.
    #[inline]
    pub fn scaling(&self) -> Option<Scaling> {
        self.scaling
    }

    /// Normalize one native sample.
    ///
    /// A **finite** result saturates into `[0,1]`. **±Infinity** saturates too: `+Inf` to
    /// `1.0`, `-Inf` to `+0.0`. **NaN** stays NaN — a NaN pixel is a real signal (a masked
    /// or dead pixel) and turning it into black data is quiet fabrication. So the output
    /// invariant is "finite samples lie in `[0,1]`", not "all samples lie in `[0,1]`".
    ///
    /// The output never carries a negative zero.
    #[inline]
    pub fn normalize<T: Sample>(&self, raw: T) -> f32 {
        // step 1 — physical value, in f64.
        //
        // The two spellings differ on no observable output; the unscaled path is a genuine
        // identity and writing it as `1.0 * x + 0.0` would invite someone to "simplify" the
        // scaled path to match. No `mul_add`, and no fused helper of any kind: the contract
        // needs the multiply and the add rounded separately.
        let physical: f64 = match self.scaling {
            Some(Scaling::Fits { bscale, bzero }) => bscale * raw.widen() + bzero,
            None => raw.widen(),
        };

        // step 2 — shift to the range low, narrowing to f32 exactly here. An f32
        // subtraction differs from the f64-then-narrow one by a ULP for a float source
        // with a nonzero declared `lo`.
        let shifted: f32 = (physical - self.range.lo) as f32;

        // step 3 — multiply by the rounded f32 reciprocal. Never a division.
        let product: f32 = shifted * self.range.k;

        saturate(product)
    }

    /// Normalize a slice of native samples into a destination, which is exactly a loop over
    /// [`Normalizer::normalize`].
    ///
    /// Element-wise autovectorization of this loop is permitted and changes no bits: Rust
    /// emits no fast-math flags, never contracts `a*b+c` into an FMA, and never
    /// reassociates. That does not extend to floating-point *reductions*, of which there
    /// are none here and none may be added.
    ///
    /// # Panics
    ///
    /// If `src` and `dst` differ in length, like [`slice::copy_from_slice`]. This is a
    /// programmer error rather than a decode outcome; the no-panic contract is about
    /// malformed *input*.
    #[inline]
    pub fn normalize_into<T: Sample>(&self, src: &[T], dst: &mut [f32]) {
        assert_eq!(
            src.len(),
            dst.len(),
            "normalize_into: source and destination lengths differ"
        );
        for (out, &raw) in dst.iter_mut().zip(src.iter()) {
            *out = self.normalize(raw);
        }
    }
}

/// Step 3's tail: a saturating clamp on ordered values, then zero normalization.
///
/// Deliberately not a `min`/`max` pair — that folds NaN to a bound, which is the outcome
/// being avoided.
#[inline]
// `clippy::manual_clamp` fires here and is wrong: `f32::clamp` folds NaN to a bound, which
// is the precise outcome this shape exists to avoid. See `docs/intentional-patterns.md`.
#[allow(clippy::manual_clamp)]
fn saturate(product: f32) -> f32 {
    let clamped = if product.is_nan() {
        product
    } else if product < 0.0f32 {
        0.0f32
    } else if product > 1.0f32 {
        1.0f32
    } else {
        product
    };

    // Two paths reach a negative zero and the clamp closes only one: a sample below `lo`
    // gives a negative product the clamp brings to the low bound, while a stored `-0.0`
    // with `lo == +0.0` passes through untouched, `-0.0` not being *below* `+0.0`. A stray
    // sign bit would trip the `to_bits()` comparisons the tests depend on. Preserve the
    // sign internally, normalize it at the boundary.
    if clamped == 0.0f32 { 0.0f32 } else { clamped }
}
