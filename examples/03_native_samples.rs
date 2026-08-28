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

        // `Samples` is the owned enum, one variant per sample width. Unlike `Bounds` and
        // `Granularity` it is deliberately **closed** — no `#[non_exhaustive]` — so this match
        // needs no wildcard and a width added later is a compile error here rather than a
        // silently-taken fallback arm. `SampleSlice` is its borrowed twin, closed for the same
        // reason; that is what the chunked path hands you (see 05_streaming).
        let summary = match &samples {
            Samples::U8(v) => integer_summary(v.iter().map(|&x| i128::from(x))),
            Samples::U16(v) => integer_summary(v.iter().map(|&x| i128::from(x))),
            Samples::U32(v) => integer_summary(v.iter().map(|&x| i128::from(x))),
            Samples::U64(v) => integer_summary(v.iter().map(|&x| i128::from(x))),
            Samples::I16(v) => integer_summary(v.iter().map(|&x| i128::from(x))),
            Samples::I32(v) => integer_summary(v.iter().map(|&x| i128::from(x))),
            Samples::I64(v) => integer_summary(v.iter().map(|&x| i128::from(x))),
            Samples::F32(v) => float_summary(v.iter().map(|&x| f64::from(x))),
            Samples::F64(v) => float_summary(v.iter().copied()),
        };
        println!(
            "{}x{}x{}  {format:?}  {summary}",
            g.width, g.height, g.channels
        );
    }
    Ok(())
}

fn integer_summary(values: impl Iterator<Item = i128>) -> String {
    let (mut min, mut max) = (i128::MAX, i128::MIN);
    for v in values {
        min = min.min(v);
        max = max.max(v);
    }
    format!("min {min}  max {max}")
}

fn float_summary(values: impl Iterator<Item = f64>) -> String {
    let (mut min, mut max, mut nans) = (f64::INFINITY, f64::NEG_INFINITY, 0usize);
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
