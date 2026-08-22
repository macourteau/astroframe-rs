//! `select_channel` and `with_bounds` — narrowing a decode, and supplying a range the file
//! does not have.
//!
//! **`with_bounds` is the escape hatch for `Bounds::Unavailable`.** A FITS float frame defines
//! no representable range, so normalized output is refused rather than invented (`01_header`
//! prints this; `02_read_image` handles it). If you know what the range should be, say so and
//! the frame normalizes. The crate will not guess it for you, because guessing is the silent
//! plausible repair this design refuses everywhere.
//!
//! This example does the two-pass version: measure the frame's own range from native samples,
//! then normalize against it. That is an autostretch, and it is **your** policy rather than the
//! crate's — which is the point of it living out here.
//!
//! **Both settings reset at every `next_image()`.** They apply to the position in force, not
//! to the reader, so a multi-image file needs them re-applied per position.
//!
//! ```text
//! cargo run --release --example 07_channels_and_bounds -- frame.fits
//! cargo run --release --example 07_channels_and_bounds -- frame.fits 1
//! ```

use astroframe::{Bounds, Reader, SampleSlice, Samples};

fn main() -> astroframe::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: 07_channels_and_bounds <frame> [channel]");
        std::process::exit(2);
    };
    let channel: Option<u32> = args.next().and_then(|s| s.parse().ok());

    // Pass 1 — the frame's own range, from native samples. This path works on every file,
    // including the ones tier 2 refuses.
    let (lo, hi) = match measure_range(&path)? {
        Some(range) => range,
        None => {
            println!("nothing to measure in {path}");
            return Ok(());
        }
    };
    println!("native range: {lo} .. {hi}");

    // Pass 2 — decode against it. A reader walks forward only, so measuring and decoding the
    // same position means opening the file twice rather than rewinding.
    let mut reader = Reader::open(&path)?;
    if !reader.next_image()? {
        return Ok(());
    }
    let header = reader.header().expect("an advanced reader has a header");
    if header.decline_reason().is_some() {
        println!("the first position is declined");
        return Ok(());
    }
    let (Some(w), Some(h), Some(c)) = (header.width(), header.height(), header.channels()) else {
        return Ok(());
    };

    // Only supply a range where the file has none. Overriding a declared one silently
    // rescales a frame whose author already said what its range was.
    if matches!(header.bounds(), Bounds::Unavailable(_)) {
        println!("the file states no range; supplying the measured one");
        reader.with_bounds(lo, hi)?;
    }

    // `select_channel` narrows the decode itself rather than slicing afterwards: with `Planar`
    // storage the unwanted channels are contiguous and get skipped outright, so this is less
    // I/O and a smaller destination, not just less output.
    let dest_len = match channel {
        Some(k) => {
            if k >= c {
                eprintln!("channel {k} is beyond the frame's {c}");
                std::process::exit(1);
            }
            reader.select_channel(k)?;
            println!("decoding channel {k} of {c}");
            w as usize * h as usize
        }
        None => w as usize * h as usize * c as usize,
    };

    let mut buffer = vec![0.0f32; dest_len];
    reader.read_image_into(&mut buffer)?;

    let finite: Vec<f32> = buffer.iter().copied().filter(|s| !s.is_nan()).collect();
    let min = finite.iter().copied().fold(f32::INFINITY, f32::min);
    let max = finite.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    println!(
        "{} samples normalized, min {min:.6} max {max:.6}",
        buffer.len()
    );
    Ok(())
}

/// The frame's own minimum and maximum, read natively so the range of a frame with no declared
/// bounds is measurable at all.
fn measure_range(path: &str) -> astroframe::Result<Option<(f64, f64)>> {
    let mut reader = Reader::open(path)?;
    if !reader.next_image()? {
        return Ok(None);
    }
    let header = reader.header().expect("an advanced reader has a header");
    if header.decline_reason().is_some() {
        return Ok(None);
    }
    let (Some(w), Some(h), Some(c), Some(format)) = (
        header.width(),
        header.height(),
        header.channels(),
        header.sample_format(),
    ) else {
        return Ok(None);
    };

    let mut samples = Samples::zeroed(format, w as usize * h as usize * c as usize);
    reader.read_samples_into(&mut samples)?;

    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    let mut take = |v: f64| {
        if !v.is_nan() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    };
    match samples.as_slice() {
        SampleSlice::U8(v) => v.iter().for_each(|&x| take(f64::from(x))),
        SampleSlice::U16(v) => v.iter().for_each(|&x| take(f64::from(x))),
        SampleSlice::U32(v) => v.iter().for_each(|&x| take(f64::from(x))),
        SampleSlice::U64(v) => v.iter().for_each(|&x| take(x as f64)),
        SampleSlice::I16(v) => v.iter().for_each(|&x| take(f64::from(x))),
        SampleSlice::I32(v) => v.iter().for_each(|&x| take(f64::from(x))),
        SampleSlice::I64(v) => v.iter().for_each(|&x| take(x as f64)),
        SampleSlice::F32(v) => v.iter().for_each(|&x| take(f64::from(x))),
        SampleSlice::F64(v) => v.iter().for_each(|&x| take(x)),
    }
    if lo.is_finite() && hi.is_finite() {
        Ok(Some((lo, hi)))
    } else {
        Ok(None)
    }
}
