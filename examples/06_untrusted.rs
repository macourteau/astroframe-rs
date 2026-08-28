//! Input you did not produce — `Limits`, and reading the error classes.
//!
//! The guarantee is that malformed input produces an `Err`, never a panic, a hang, or
//! unbounded allocation. Every size a header declares is a cross-check rather than an
//! allocation size, and `Limits` bounds the geometry that is not.
//!
//! **`Limits` is a runtime knob, not a compile-time one**, and the defaults are sized for
//! frames you produced yourself. For a hostile source, lower them. The figure worth knowing:
//! caps multiply, so the largest allocation a header alone can provoke at the defaults is
//! about **1 GB**, measured. `Images per source` (256) times `Assembled keyword value` (1 MiB)
//! is 256 MiB of assembled text; the intermediate copies the assembly makes on the way there
//! are what carry it to a gigabyte. See § The caps. That is bounded and finite, and it is far
//! more than a service accepting uploads should hand an attacker.
//!
//! ```text
//! cargo run --example 06_untrusted -- suspect.fits
//! ```

use astroframe::{Error, Limits, Reader};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: 06_untrusted <frame>");
        std::process::exit(2);
    };

    // Start from the defaults and tighten; `Limits` is `#[non_exhaustive]`, so it is always
    // built this way and a cap added in a later version is an addition rather than a break.
    // The caps a caller commonly moves have chainable setters; the rest are public fields.
    let mut limits = Limits::default()
        // Refuse a mosaic rather than walk 256 positions. Raise it if you have real MEFs.
        .with_images_per_source(8)
        // Bound the decode itself to something a request handler can afford. These are
        // genuinely tight: over a 1242-frame corpus of real astronomical data they refuse
        // about 8% of it, almost all of that XISF masters whose compressed block must be held
        // whole. That is the trade this example exists to make visible — a cap you never feel
        // is a cap that is not doing anything.
        .with_total_samples(64 << 20) // 64 megapixels
        .with_decoded_output_bytes(256 << 20) // the f32 destination
        .with_materialized_bytes(256 << 20); // scratch this crate allocates for itself

    // The single most effective knob against the cap-product figure above: the ceiling is
    // linear in it, so 64 KiB takes ~1 GB to ~64 MB. The long strings the FITS `CONTINUE`
    // convention exists for — a WCS solution, a HISTORY trail — are kilobytes, so this costs
    // nothing real.
    limits.keyword_value_bytes = 64 << 10;
    limits.attribute_value_bytes = 64 << 10;

    match decode(&path, limits) {
        Ok(images) => println!("decoded {images} image(s) from {path}"),
        Err(e) => {
            // The class is the actionable part, and each means something different for what a
            // caller should do next. Matching on it beats matching on the message.
            //
            // The class itself is not repeated here: every variant's `Display` already opens
            // with it, so a hand-written label beside `{e}` would be the enum spelled twice,
            // and the copy that drifts is always the hand-written one.
            let action = match &e {
                Error::Io(_) => "the source could not be read — retry or report",
                Error::Malformed(_) => "the file is not valid — reject it",
                Error::Unsupported(_) => "valid, but this version does not read it — keep the file",
                Error::ChecksumMismatch(_) => "the file says its own bytes are wrong — reject it",
                Error::LimitExceeded(_) => "a cap tripped — raise it deliberately, or refuse",
                Error::InvalidRequest(_) => {
                    "this program asked for something impossible — a bug here"
                }
                // `Error` is `#[non_exhaustive]`: a class added later lands here rather than
                // failing to compile. Refusing is the safe default for a class this program
                // has never seen.
                _ => "unrecognized class — refuse the source",
            };
            eprintln!("{e}");
            eprintln!("  -> {action}");
            std::process::exit(1);
        }
    }
}

fn decode(path: &str, limits: Limits) -> astroframe::Result<usize> {
    let mut reader = Reader::open_with_limits(path, limits)?;
    let mut decoded = 0usize;
    let mut buffer: Vec<f32> = Vec::new();

    while reader.next_image()? {
        let header = reader.current_header()?;

        // A decline is not an error: this position is valid and unreadable by this version,
        // and the rest of the file still walks. Treating it as a failure throws away the
        // images that *do* decode — see the multi-image case in 01_header.
        if let Some(decline) = header.decline_reason() {
            eprintln!("  skipping a position: {decline}");
            continue;
        }
        if header.geometry().is_none() {
            continue;
        }
        buffer.clear();
        buffer.resize(reader.destination_len()?, 0.0);

        // `Unsupported` gets the same treatment as a decline, for the same reason and against
        // this program's own table below: it means "valid, but this version does not read it",
        // so aborting the walk throws away the positions that *do* decode. Run over a real
        // corpus this fires on every FITS float master — 13 of 1242 frames here — which is a
        // perfectly good calibration frame, not an attack.
        match reader.read_image_into(&mut buffer) {
            Ok(()) => decoded += 1,
            Err(Error::Unsupported(why)) => {
                eprintln!("  skipping a position: {why}");
                continue;
            }
            // Everything else is the file being wrong or a cap tripping, and both should stop
            // this source rather than be counted as progress.
            Err(e) => return Err(e),
        }
    }
    Ok(decoded)
}
