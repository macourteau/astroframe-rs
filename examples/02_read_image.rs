//! Tier 2 — decode a whole image to normalized `f32`.
//!
//! There are two calls, and this example uses the second one. `read_image()` allocates the
//! destination and hands back an `Image`, which is the one-frame shape:
//!
//! ```text
//! let (header, samples) = reader.read_image()?.into_parts();
//! ```
//!
//! `read_image_into(&mut [f32])` writes into a destination you own, which is what a loop over
//! many frames wants: one buffer, resized in place, instead of an allocation per frame. That
//! is the shape below.
//!
//! ```text
//! cargo run --release --example 02_read_image -- frame.xisf
//! ```
//!
//! Build with `--release`. The debug profile decodes megapixel frames slowly enough to be
//! misleading about this crate's speed.

use astroframe::{Error, Reader};

fn main() -> astroframe::Result<()> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: 02_read_image <frame.fits|frame.xisf>");
        std::process::exit(2);
    };

    let mut reader = Reader::open(&path)?;
    let mut buffer: Vec<f32> = Vec::new();

    while reader.next_image()? {
        let header = reader.current_header()?;
        if header.decline_reason().is_some() {
            continue;
        }

        // The geometry is `Option` — a declined position has no geometry, and a malformed one
        // may be missing pieces. Past `decline_reason` it is present, and the three axes move
        // as a unit, so it is one `Option` rather than three.
        let Some(g) = header.geometry() else {
            continue;
        };
        let (w, h, c) = (g.width, g.height, g.channels);

        // Reuse one buffer across frames. `read_image_into` requires an exactly-sized
        // destination and returns `InvalidRequest` otherwise, rather than silently truncating,
        // and `destination_len` is that same count computed by the reader. `clear` before
        // `resize` so a grown buffer does not copy samples the decode is about to overwrite.
        buffer.clear();
        buffer.resize(reader.destination_len()?, 0.0);

        // Not every frame has a representable range — a FITS float frame typically does not,
        // and normalized output is refused rather than invented. That is `Unsupported`, and it
        // is a property of the file rather than a failure, so handle it instead of dying on a
        // frame the next tier down reads perfectly well. `03_native_samples` is that tier;
        // `07_channels_and_bounds` shows `set_bounds`, the escape hatch.
        //
        // `match`, not `if let`. An `if let Err(Unsupported)` here reads fine and is wrong:
        // every other class falls through it, and the summary below then prints statistics for
        // a buffer the decode never filled — a truncated frame comes out as a clean row of
        // zeros and an exit code of 0. Swallowing an error is worse here than anywhere,
        // because the thing that comes out the other side looks like an answer.
        match reader.read_image_into(&mut buffer) {
            Ok(()) => {}
            Err(Error::Unsupported(why)) => {
                println!("{w}x{h}x{c}  no normalized output: {why}");
                continue;
            }
            Err(e) => return Err(e),
        }

        let stats = summarize(&buffer);
        println!(
            "{w}x{h}x{c}  min {:.6}  max {:.6}  mean {:.6}  NaN {}",
            stats.min, stats.max, stats.mean, stats.nans
        );
    }
    Ok(())
}

struct Summary {
    min: f32,
    max: f32,
    mean: f64,
    nans: usize,
}

/// Normalized output is in `[0, 1]` by construction, so only the NaNs need a decision.
///
/// NaN is passed through from float sources rather than repaired, which leaves the policy
/// here: count them, because a silent skip is how a bad frame looks fine.
fn summarize(samples: &[f32]) -> Summary {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f64;
    let mut nans = 0usize;
    for &s in samples {
        if s.is_nan() {
            nans += 1;
            continue;
        }
        min = min.min(s);
        max = max.max(s);
        sum += f64::from(s);
    }
    let finite = samples.len() - nans;
    Summary {
        min,
        max,
        mean: if finite > 0 {
            sum / finite as f64
        } else {
            f64::NAN
        },
        nans,
    }
}
