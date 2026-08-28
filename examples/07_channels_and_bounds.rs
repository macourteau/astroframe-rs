//! `select_channel` and `set_bounds` — narrowing a decode, and supplying a range the file
//! does not have.
//!
//! **`set_bounds` is the escape hatch for `Bounds::Unavailable`.** A FITS float frame defines
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

use astroframe::{Bounds, Reader};

fn main() -> astroframe::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: 07_channels_and_bounds <frame> [channel]");
        std::process::exit(2);
    };
    // Parsed rather than best-effort: `parse().ok()` would turn a typo into "decode every
    // channel", which is a silent wrong answer of exactly the kind this crate refuses to
    // produce from a file.
    let channel: Option<u32> = match args.next().map(|arg| arg.parse()) {
        Some(Ok(k)) => Some(k),
        Some(Err(e)) => {
            eprintln!("channel: {e}");
            std::process::exit(2);
        }
        None => None,
    };

    // Pass 1 — the frame's own range, from native samples. This path works on every file,
    // including the ones tier 2 refuses.
    let Some((lo, hi)) = measure_range(&path)? else {
        println!("nothing to measure in {path}");
        return Ok(());
    };
    println!("native range: {lo} .. {hi}");

    // Pass 2 — decode against it. A reader walks forward only, so measuring and decoding the
    // same position means opening the file twice rather than rewinding.
    let mut reader = Reader::open(&path)?;
    if !reader.next_image()? {
        return Ok(());
    }
    let header = reader.current_header()?;
    if header.decline_reason().is_some() {
        println!("the first position is declined");
        return Ok(());
    }
    let Some(g) = header.geometry() else {
        return Ok(());
    };

    // Only supply a range where the file has none. Overriding a declared one silently
    // rescales a frame whose author already said what its range was.
    if matches!(header.bounds(), Bounds::Unavailable(_)) {
        println!("the file states no range; supplying the measured one");
        reader.set_bounds(lo, hi)?;
    }

    // `select_channel` narrows the decode itself rather than slicing afterwards: with `Planar`
    // storage the unwanted channels are contiguous and get skipped outright, so this is less
    // I/O and a smaller destination, not just less output.
    if let Some(k) = channel {
        if k >= g.channels {
            eprintln!("channel {k} is beyond the frame's {}", g.channels);
            std::process::exit(1);
        }
        reader.select_channel(k)?;
        println!("decoding channel {k} of {}", g.channels);
    }

    // Sized by the reader, **after** the configuration above: `destination_len` reports what
    // this reader will produce, so narrowing shrinks it and no rule about which header to
    // measure has to be remembered.
    let mut buffer = vec![0.0f32; reader.destination_len()?];
    reader.read_image_into(&mut buffer)?;

    // One pass, no second buffer: a megapixel frame does not need a copy of itself to be
    // summarized, and `fold` over the filtered iterator is the same arithmetic without one.
    let (min, max) = buffer
        .iter()
        .copied()
        .filter(|s| !s.is_nan())
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), s| {
            (lo.min(s), hi.max(s))
        });
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
    let header = reader.current_header()?;
    if header.decline_reason().is_some() {
        return Ok(None);
    }
    if header.geometry().is_none() || header.sample_format().is_none() {
        return Ok(None);
    }

    let samples = reader.read_samples()?;

    // `iter_f64` is the crate's own widening — `Sample::widen`, the step the normalization
    // itself performs — applied to whichever width the file holds, so measuring a range needs
    // no match over the nine variants and introduces no rounding of its own.
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for value in samples.as_slice().iter_f64() {
        if !value.is_nan() {
            lo = lo.min(value);
            hi = hi.max(value);
        }
    }
    if lo.is_finite() && hi.is_finite() {
        Ok(Some((lo, hi)))
    } else {
        Ok(None)
    }
}
