//! Tier 1 — everything you can learn without decoding a pixel.
//!
//! Constructing a `Reader` and reading a header reads **no pixel byte**. For a metadata sweep
//! over a night's frames that is the whole job, and it is the same code for FITS and XISF.
//!
//! ```text
//! cargo run --example 01_header -- frame.fits
//! ```

use astroframe::{Bounds, Reader, RowOrder};

fn main() -> astroframe::Result<()> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: 01_header <frame.fits|frame.xisf>");
        std::process::exit(2);
    };

    let mut reader = Reader::open(&path)?;

    // A file holds one or more image *positions*. FITS calls them HDUs, XISF calls them
    // <Image> elements; `next_image` walks both and returns false at the end.
    let mut position = 0;
    while reader.next_image()? {
        // Past a successful advance a header always exists, so this is a `Result` rather than
        // an `Option`: the error says "no image is selected", which is a mistake this loop
        // cannot make.
        let header = reader.current_header()?;
        position += 1;
        println!("--- position {position} ---");

        // A position this version will not decode says so, in a class and a sentence, rather
        // than by erroring. The rest of the file still walks. Check this before anything else:
        // the geometry accessors below may be `None` here.
        if let Some(decline) = header.decline_reason() {
            println!("  declined   {decline}");
            continue;
        }

        // The three axes move as a unit — all present or all absent — so they are read as
        // one value rather than as three `Option`s a caller has to reassemble.
        match header.geometry() {
            Some(g) => println!("  geometry   {} x {} x {}", g.width, g.height, g.channels),
            None => println!("  geometry   incomplete"),
        }
        if let Some(format) = header.sample_format() {
            println!("  samples    {format:?} ({} bytes each)", format.bytes());
        }

        // The range normalized output is computed against, and where it came from. This is
        // reported rather than guessed at, which is what lets a caller tell "the file said so"
        // from "the format's default applied" from "there is no usable range here". Each
        // usable variant carries the validated `Range` the decode will actually use.
        match header.bounds() {
            Bounds::FormatDefault(r) => {
                println!("  bounds     {}..{} (format default)", r.lo(), r.hi());
            }
            Bounds::Declared(r) => {
                println!("  bounds     {}..{} (declared by the file)", r.lo(), r.hi());
            }
            Bounds::CallerSupplied { .. } => println!("  bounds     supplied by this caller"),
            Bounds::Unavailable(why) => {
                // Not a rejection: native samples still decode (see 03_native_samples).
                println!("  bounds     unavailable ({why:?}) — normalized output is refused");
            }
            // `Bounds` is `#[non_exhaustive]`, so a wildcard arm is required and a future
            // variant will not break this build. The same is true of most enums here.
            other => println!("  bounds     {other:?}"),
        }

        // How much of the input the decoder must hold before it can produce any sample. Worth
        // reading *before* deciding to stream — see 05_streaming.
        println!("  streaming  {:?}", header.granularity());

        // Printed through `Display`, which is the file's own spelling: `BOTTOM-UP`, not the
        // Rust name. That is what a consumer re-emitting the fact writes. `Unspecified` is
        // the absent keyword and has no spelling, so there is nothing to report.
        match header.row_order() {
            None | Some(RowOrder::Unspecified) => {}
            Some(order) => println!("  row order  {order}"),
        }
        if let Some(cfa) = header.cfa() {
            println!(
                "  CFA        {} ({}x{})",
                cfa.pattern(),
                cfa.width(),
                cfa.height()
            );
        }
        println!("  keywords   {}", header.keywords().len());
        println!("  properties {}", header.properties().len());
    }

    if position == 0 {
        println!("no image positions in {path}");
    }
    Ok(())
}
