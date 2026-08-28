//! Native samples — what the file actually holds, before normalization.

/// The scalar width a container stores its samples in.
///
/// The signed variants exist for FITS, where `BITPIX` 16, 32 and 64 store *signed* integers;
/// `BITPIX = 8` is unsigned and XISF has no signed formats at all, so no source can produce
/// an `I8`. XISF's complex formats are absent by design rather than by omission — see
/// [`Samples`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleFormat {
    /// 8-bit unsigned.
    U8,
    /// 16-bit unsigned.
    U16,
    /// 32-bit unsigned.
    U32,
    /// 64-bit unsigned.
    U64,
    /// 16-bit signed.
    I16,
    /// 32-bit signed.
    I32,
    /// 64-bit signed.
    I64,
    /// IEEE-754 binary32.
    F32,
    /// IEEE-754 binary64.
    F64,
}

impl SampleFormat {
    /// Bytes per stored sample.
    pub fn bytes(self) -> u32 {
        match self {
            SampleFormat::U8 => 1,
            SampleFormat::U16 | SampleFormat::I16 => 2,
            SampleFormat::U32 | SampleFormat::I32 | SampleFormat::F32 => 4,
            SampleFormat::U64 | SampleFormat::I64 | SampleFormat::F64 => 8,
        }
    }

    /// Whether samples are integers rather than floats.
    pub fn is_integer(self) -> bool {
        !matches!(self, SampleFormat::F32 | SampleFormat::F64)
    }
}

/// Native samples, owned.
///
/// An enum over exactly the scalar widths the two formats can store, which is what makes a
/// complex sample format **unrepresentable** in this crate's output rather than merely
/// unimplemented.
///
/// Deliberately **not** `#[non_exhaustive]`, unlike every other public enum here: the set is
/// closed by the formats themselves, and a caller matching on it exhaustively is doing the
/// right thing. Widening it later would widen every consumer's match, which is precisely the
/// cost the design declines to take on for data no consumer reads.
///
/// It owns its buffers, and `read_samples_into` fills the buffer already inside the variant
/// the caller passes — which is what makes "allocate once and reuse across frames" true at
/// layer 1 as well as layer 2.
#[derive(Clone, Debug, PartialEq)]
pub enum Samples {
    /// 8-bit unsigned samples.
    U8(Vec<u8>),
    /// 16-bit unsigned samples.
    U16(Vec<u16>),
    /// 32-bit unsigned samples.
    U32(Vec<u32>),
    /// 64-bit unsigned samples.
    U64(Vec<u64>),
    /// 16-bit signed samples.
    I16(Vec<i16>),
    /// 32-bit signed samples.
    I32(Vec<i32>),
    /// 64-bit signed samples.
    I64(Vec<i64>),
    /// `f32` samples.
    F32(Vec<f32>),
    /// `f64` samples.
    F64(Vec<f64>),
}

/// Run one expression over whichever variant an owned or borrowed sample enum holds.
macro_rules! samples_dispatch {
    ($enum:ident, $self:expr, $v:ident => $body:expr) => {
        match $self {
            $enum::U8($v) => $body,
            $enum::U16($v) => $body,
            $enum::U32($v) => $body,
            $enum::U64($v) => $body,
            $enum::I16($v) => $body,
            $enum::I32($v) => $body,
            $enum::I64($v) => $body,
            $enum::F32($v) => $body,
            $enum::F64($v) => $body,
        }
    };
}

impl Samples {
    /// A zeroed buffer of `len` samples in `format`, ready to hand to `read_samples_into`.
    pub fn zeroed(format: SampleFormat, len: usize) -> Samples {
        match format {
            SampleFormat::U8 => Samples::U8(vec![0; len]),
            SampleFormat::U16 => Samples::U16(vec![0; len]),
            SampleFormat::U32 => Samples::U32(vec![0; len]),
            SampleFormat::U64 => Samples::U64(vec![0; len]),
            SampleFormat::I16 => Samples::I16(vec![0; len]),
            SampleFormat::I32 => Samples::I32(vec![0; len]),
            SampleFormat::I64 => Samples::I64(vec![0; len]),
            SampleFormat::F32 => Samples::F32(vec![0.0; len]),
            SampleFormat::F64 => Samples::F64(vec![0.0; len]),
        }
    }

    /// Which variant this is.
    pub fn format(&self) -> SampleFormat {
        match self {
            Samples::U8(_) => SampleFormat::U8,
            Samples::U16(_) => SampleFormat::U16,
            Samples::U32(_) => SampleFormat::U32,
            Samples::U64(_) => SampleFormat::U64,
            Samples::I16(_) => SampleFormat::I16,
            Samples::I32(_) => SampleFormat::I32,
            Samples::I64(_) => SampleFormat::I64,
            Samples::F32(_) => SampleFormat::F32,
            Samples::F64(_) => SampleFormat::F64,
        }
    }

    /// How many samples the buffer holds.
    pub fn len(&self) -> usize {
        samples_dispatch!(Samples, self, v => v.len())
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Borrow the whole buffer as a [`SampleSlice`].
    ///
    /// The two enums mirror each other — owned and borrowed — and this is what lets code
    /// written against one take the other. That matters because the two halves of the API hand
    /// you different ones: [`crate::Reader::read_samples_into`] fills a `Samples` you own,
    /// while [`crate::Chunk::samples`] lends a `SampleSlice`, so a routine meant to serve both
    /// tiers would otherwise need its match duplicated.
    ///
    /// ```
    /// use astroframe::{SampleFormat, SampleSlice, Samples};
    ///
    /// let owned = Samples::zeroed(SampleFormat::U16, 3);
    /// assert!(matches!(owned.as_slice(), SampleSlice::U16(v) if v.len() == 3));
    /// ```
    pub fn as_slice(&self) -> SampleSlice<'_> {
        match self {
            Samples::U8(v) => SampleSlice::U8(v),
            Samples::U16(v) => SampleSlice::U16(v),
            Samples::U32(v) => SampleSlice::U32(v),
            Samples::U64(v) => SampleSlice::U64(v),
            Samples::I16(v) => SampleSlice::I16(v),
            Samples::I32(v) => SampleSlice::I32(v),
            Samples::I64(v) => SampleSlice::I64(v),
            Samples::F32(v) => SampleSlice::F32(v),
            Samples::F64(v) => SampleSlice::F64(v),
        }
    }
}

/// A borrowed run of native samples, as delivered to a chunk consumer.
///
/// Closed for the same reason [`Samples`] is: it mirrors that enum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleSlice<'a> {
    /// 8-bit unsigned samples.
    U8(&'a [u8]),
    /// 16-bit unsigned samples.
    U16(&'a [u16]),
    /// 32-bit unsigned samples.
    U32(&'a [u32]),
    /// 64-bit unsigned samples.
    U64(&'a [u64]),
    /// 16-bit signed samples.
    I16(&'a [i16]),
    /// 32-bit signed samples.
    I32(&'a [i32]),
    /// 64-bit signed samples.
    I64(&'a [i64]),
    /// `f32` samples.
    F32(&'a [f32]),
    /// `f64` samples.
    F64(&'a [f64]),
}

impl<'a> SampleSlice<'a> {
    /// The run as one sample width, or `None` when it holds another.
    ///
    /// The nine-arm match written once, for a consumer that has already read
    /// [`SampleSlice::format`] and wants the slice typed.
    ///
    /// ```
    /// use astroframe::{SampleFormat, SampleSlice, Samples};
    ///
    /// let owned = Samples::zeroed(SampleFormat::U16, 3);
    /// assert_eq!(owned.as_slice().try_as::<u16>().map(<[u16]>::len), Some(3));
    /// assert_eq!(owned.as_slice().try_as::<f32>(), None);
    /// ```
    pub fn try_as<T: crate::normalize::Sample>(self) -> Option<&'a [T]> {
        <T as crate::normalize::sealed::Sealed>::from_slice(self)
    }

    /// Every sample widened to `f64`, whatever width the run holds.
    ///
    /// The one operation a consumer summarizing native samples always ends up writing by
    /// hand, and it is [`Sample::widen`](crate::Sample::widen) — the same widening step 1 of
    /// the normalization performs — applied element by element, so no rounding enters that
    /// was not already in the contract. For `U64` and `I64` it is lossy above 2⁵³, exactly as
    /// `widen` is.
    ///
    /// ```
    /// use astroframe::{SampleFormat, Samples};
    ///
    /// let owned = Samples::U16(vec![0, 32768, 65535]);
    /// let widened: Vec<f64> = owned.as_slice().iter_f64().collect();
    /// assert_eq!(widened, [0.0, 32768.0, 65535.0]);
    /// ```
    pub fn iter_f64(self) -> impl Iterator<Item = f64> + 'a {
        WidenIter { slice: self, at: 0 }
    }

    /// Which variant this is.
    pub fn format(&self) -> SampleFormat {
        match self {
            SampleSlice::U8(_) => SampleFormat::U8,
            SampleSlice::U16(_) => SampleFormat::U16,
            SampleSlice::U32(_) => SampleFormat::U32,
            SampleSlice::U64(_) => SampleFormat::U64,
            SampleSlice::I16(_) => SampleFormat::I16,
            SampleSlice::I32(_) => SampleFormat::I32,
            SampleSlice::I64(_) => SampleFormat::I64,
            SampleSlice::F32(_) => SampleFormat::F32,
            SampleSlice::F64(_) => SampleFormat::F64,
        }
    }

    /// How many samples the run holds.
    pub fn len(&self) -> usize {
        samples_dispatch!(SampleSlice, self, v => v.len())
    }

    /// Whether the run is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What [`SampleSlice::iter_f64`] returns, kept private so the crate is free to change it.
///
/// A cursor over the borrowed run rather than nine chained `map` iterators behind a
/// `Box<dyn Iterator>`: the boxed spelling is shorter and costs one allocation per call, which
/// a per-chunk caller pays once per chunk for no gain.
#[derive(Debug)]
struct WidenIter<'a> {
    slice: SampleSlice<'a>,
    at: usize,
}

impl Iterator for WidenIter<'_> {
    type Item = f64;

    fn next(&mut self) -> Option<f64> {
        use crate::normalize::Sample;

        let at = self.at;
        let widened =
            samples_dispatch!(SampleSlice, self.slice, v => v.get(at).map(|s| Sample::widen(*s)))?;
        self.at = at + 1;
        Some(widened)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.slice.len() - self.at;
        (left, Some(left))
    }
}

impl ExactSizeIterator for WidenIter<'_> {}

/// One row's samples, sliced to the caller's declared chunk length.
///
/// `len` is always the width `Samples::zeroed` sized `s` to when the chunk was built, so this
/// indexes directly rather than truncating to whichever is smaller: a mismatch is a decoder
/// bug, and handing back fewer samples than the chunk claims is the same silent repair the
/// pixel path refuses everywhere else. It lives here rather than in either decoder because
/// both need exactly this and it is about `Samples` and `SampleSlice`, which this module owns.
// Gated on the formats, not unconditional: both callers are decoders, so with neither format
// compiled in this is dead — and § Operations makes the empty feature set a supported build.
#[cfg(any(feature = "fits", feature = "xisf"))]
pub(crate) fn slice_samples(s: &Samples, len: usize) -> SampleSlice<'_> {
    match s {
        Samples::U8(v) => SampleSlice::U8(&v[..len]),
        Samples::U16(v) => SampleSlice::U16(&v[..len]),
        Samples::U32(v) => SampleSlice::U32(&v[..len]),
        Samples::U64(v) => SampleSlice::U64(&v[..len]),
        Samples::I16(v) => SampleSlice::I16(&v[..len]),
        Samples::I32(v) => SampleSlice::I32(&v[..len]),
        Samples::I64(v) => SampleSlice::I64(&v[..len]),
        Samples::F32(v) => SampleSlice::F32(&v[..len]),
        Samples::F64(v) => SampleSlice::F64(&v[..len]),
    }
}

#[cfg(all(test, any(feature = "fits", feature = "xisf")))]
mod slice_tests {
    use super::*;

    /// A `len` above the buffer is a decoder bug, and the answer to those is a hard failure
    /// rather than a short slice — a chunk handing back fewer samples than it claims corrupts
    /// the caller's image silently. Both decoders relied on this and each carried its own copy
    /// of the check and of this test.
    #[test]
    #[should_panic(expected = "range end index")]
    fn slice_samples_fails_hard_on_a_length_that_exceeds_the_buffer() {
        let s = Samples::U8(vec![1, 2, 3]);
        let _ = slice_samples(&s, 4);
    }
}
