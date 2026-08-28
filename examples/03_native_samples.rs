//! Native samples — decoding without normalizing.
//!
//! Two reasons to prefer this over tier 2:
//!
//! 1. **Some frames have no representable range**, so normalized output is refused — a FITS
//!    float frame is the common case, and `01_header` prints `Unavailable(NoFormatDefault)`
//!    for exactly that. Native samples still decode. This is the path that reads every file.
//! 2. You want the file's own integers, not a rescaling of them.
//!
//! ```text
//! cargo run --release --example 03_native_samples -- frame.fits
//! ```

use astroframe::{Reader, Samples};

fn main() -> astroframe::Result<()> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: 03_native_samples <frame.fits|frame.xisf>");
        std::process::exit(2);
    };

    let mut reader = Reader::open(&path)?;

    while reader.next_image()? {
        let header = reader.current_header()?;
        if header.decline_reason().is_some() {
            continue;
        }
        let (Some(g), Some(format)) = (header.geometry(), header.sample_format()) else {
            continue;
        };

        // The destination is typed by the file's own format rather than chosen by the caller:
        // handing `read_samples_into` a mismatched variant is `InvalidRequest`, not a
        // conversion. `read_samples` allocates one of the right variant and the right length,
        // which is the layer-1 counterpart of `read_image`; over many frames prefer
        // `Samples::zeroed` once and `read_samples_into` in the loop.
        let samples = reader.read_samples()?;

        println!(
            "{}x{}x{}  {format:?}  {}",
            g.width,
            g.height,
            g.channels,
            summarize(&samples)
        );
    }
    Ok(())
}

/// Widest range in the file's own units, which means matching over the sample widths.
///
/// `Samples` is the owned enum, one variant per sample width. Unlike `Bounds` and
/// `Granularity` it is deliberately **closed** — no `#[non_exhaustive]` — so this match needs
/// no wildcard and a width added later is a compile error here rather than a silently-taken
/// fallback arm. `SampleSlice` is its borrowed twin, closed for the same reason; that is what
/// the chunked path hands you (see `05_streaming`).
///
/// Matching is what a caller wanting the file's *integers* has to do, because there is no one
/// type they all fit in. When a `f64` will do — measuring a range, say — `SampleSlice::iter_f64`
/// widens every variant through the same step the normalization itself uses and skips the
/// match entirely; `07_channels_and_bounds` takes that path.
fn summarize(samples: &Samples) -> String {
    match samples {
        Samples::U8(v) => integer_range(v.iter().map(|&x| i128::from(x))),
        Samples::U16(v) => integer_range(v.iter().map(|&x| i128::from(x))),
        Samples::U32(v) => integer_range(v.iter().map(|&x| i128::from(x))),
        Samples::U64(v) => integer_range(v.iter().map(|&x| i128::from(x))),
        Samples::I16(v) => integer_range(v.iter().map(|&x| i128::from(x))),
        Samples::I32(v) => integer_range(v.iter().map(|&x| i128::from(x))),
        Samples::I64(v) => integer_range(v.iter().map(|&x| i128::from(x))),
        Samples::F32(v) => float_range(v.iter().map(|&x| f64::from(x))),
        Samples::F64(v) => float_range(v.iter().copied()),
    }
}

/// `i128` holds every integer variant including `u64`, so the widening loses nothing.
fn integer_range(values: impl Iterator<Item = i128>) -> String {
    let (min, max) = values.fold((i128::MAX, i128::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    format!("min {min}  max {max}")
}

/// NaN is passed through from float sources rather than repaired, so it is kept out of the
/// range and counted instead — a silent skip is how a bad frame looks fine.
fn float_range(values: impl Iterator<Item = f64>) -> String {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut nans = 0usize;
    for v in values {
        if v.is_nan() {
            nans += 1;
            continue;
        }
        min = min.min(v);
        max = max.max(v);
    }
    format!("min {min:.6}  max {max:.6}  NaN {nans}")
}
