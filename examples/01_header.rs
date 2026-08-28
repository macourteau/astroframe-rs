//! Tier 1 — everything you can learn without decoding a pixel.
//!
//! Constructing a `Reader` and reading a header reads **no pixel byte**. For a metadata sweep
//! over a night's frames that is the whole job, and it is the same code for FITS and XISF.
//!
//! ```text
//! cargo run --example 01_header -- frame.fits
//! ```

use astroframe::{Bounds, Header, Reader, RowOrder};
use std::fmt::Display;

fn main() -> astroframe::Result<()> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: 01_header <frame.fits|frame.xisf>");
        std::process::exit(2);
    };

    let mut reader = Reader::open(&path)?;

    // Which container this is, decided by the leading bytes at construction rather than by the
    // extension, and answerable before the first `next_image`. `Header::format` reports the
    // same fact per position, for code holding only a header.
    println!("{path}: {}", reader.format());

    // A file holds one or more image *positions*. FITS calls them HDUs, XISF calls them
    // <Image> elements; `next_image` walks both and returns false at the end.
    let mut positions = 0;
    while reader.next_image()? {
        // Past a successful advance a header always exists, so this is a `Result` rather than
        // an `Option`: the error says "no image is selected", which is a mistake this loop
        // cannot make.
        let header = reader.current_header()?;
        positions += 1;
        println!("--- position {positions} ---");
        describe(&header);
    }

    if positions == 0 {
        println!("no image positions in {path}");
    }
    Ok(())
}

fn describe(header: &Header) {
    // A position this version will not decode says so, in a class and a sentence, rather than
    // by erroring. The rest of the file still walks. Check this before anything else: the
    // geometry accessors below may be `None` here.
    if let Some(decline) = header.decline_reason() {
        println!("  declined    {decline}");
        return;
    }

    // The three axes move as a unit — all present or all absent — so they are read as one
    // value rather than as three `Option`s a caller has to reassemble.
    match header.geometry() {
        Some(g) => println!("  geometry    {} x {} x {}", g.width, g.height, g.channels),
        None => println!("  geometry    incomplete"),
    }
    if let Some(format) = header.sample_format() {
        println!("  samples     {format:?} ({} bytes each)", format.bytes());
    }

    // The range normalized output is computed against, and where it came from. This is
    // reported rather than guessed at, which is what lets a caller tell "the file said so"
    // from "the format's default applied" from "there is no usable range here". Each usable
    // variant carries the validated `SampleRange` the decode will actually use.
    let bounds = match header.bounds() {
        Bounds::FormatDefault(r) => format!("{}..{} (format default)", r.lo(), r.hi()),
        Bounds::Declared(r) => format!("{}..{} (declared by the file)", r.lo(), r.hi()),
        Bounds::CallerSupplied { effective, .. } => {
            format!(
                "{}..{} (supplied by a caller)",
                effective.lo(),
                effective.hi()
            )
        }
        // Not a rejection: native samples still decode (see 03_native_samples).
        Bounds::Unavailable(why) => format!("unavailable ({why:?}) — normalization is refused"),
        // `Bounds` is `#[non_exhaustive]`, so a wildcard arm is required and a future variant
        // will not break this build. The same is true of most enums here.
        other => format!("{other:?}"),
    };
    println!("  bounds      {bounds}");

    // How much of the input the decoder must hold before it can produce any sample. Worth
    // reading *before* deciding to stream — see 05_streaming.
    println!("  streaming   {:?}", header.granularity());

    // Everything below is optional and prints through `Display`, which is the file's own
    // spelling — `BOTTOM-UP`, not the Rust name — because that is what a consumer re-emitting
    // the fact writes. `RowOrder::Unspecified` is the absent keyword and has no spelling.
    let stated = header
        .row_order()
        .filter(|order| !matches!(order, RowOrder::Unspecified));
    report("row order", stated);
    report("orientation", header.orientation());
    report("image type", header.image_type());
    report("color", header.color_space());
    report("storage", header.pixel_storage());
    if let Some(cfa) = header.cfa() {
        println!(
            "  CFA         {} ({}x{})",
            cfa.pattern(),
            cfa.width(),
            cfa.height()
        );
    }

    println!("  keywords    {}", header.keywords().len());
    println!("  properties  {}", header.properties().len());
}

/// One line per fact the file stated, and silence for the ones it did not.
///
/// Each of these is an `Option` because the format may define no such thing or the file may
/// have omitted it, and both mean "nothing to re-emit" rather than "missing". Printing `None`
/// for a FITS frame's `orientation` would report an XISF concept as a defect.
fn report(label: &str, value: Option<impl Display>) {
    if let Some(value) = value {
        println!("  {label:<11} {value}");
    }
}
