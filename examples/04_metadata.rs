//! Keywords and properties — the metadata surface.
//!
//! The organizing rule is **report, don't interpret**: the crate hands you what the file said,
//! in the file's own spelling, and translating it is your parse step. So a FITS `DATE-OBS`
//! comes back as the text on the card, not as a timestamp type.
//!
//! Both containers are reported for both formats. An XISF file carrying a `<FITSKeyword>` block
//! reports it under `keywords()` exactly as a FITS file would, so a consumer that only knows
//! FITS keywords works across both.
//!
//! ```text
//! cargo run --example 04_metadata -- frame.xisf
//! cargo run --example 04_metadata -- frame.xisf DATE-OBS EXPTIME
//! ```

use astroframe::{Header, Reader};

/// How many entries to print before summarizing the rest. A processed frame carries hundreds
/// of `HISTORY` cards and dumping them all buries everything else.
const PREVIEW: usize = 20;

fn main() -> astroframe::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: 04_metadata <frame> [name...]");
        std::process::exit(2);
    };
    let wanted: Vec<String> = args.collect();

    let mut reader = Reader::open(&path)?;
    if !reader.next_image()? {
        println!("no image positions in {path}");
        return Ok(());
    }
    let header = reader.current_header()?;

    if wanted.is_empty() {
        dump(&header);
    } else {
        look_up(&header, &wanted);
    }
    Ok(())
}

/// Everything the first position reports, truncated to something readable.
fn dump(header: &Header) {
    // Keywords are reported in file order, duplicates and all — a FITS header may carry many
    // `HISTORY` cards and each is its own entry.
    let keywords = header.keywords();
    println!("keywords ({}):", keywords.len());
    for keyword in keywords.iter().take(PREVIEW) {
        // The comment is `None` for a FITS card carrying none, which is not the same as an
        // empty one, so the separator goes with the text rather than being printed either way.
        let comment = keyword
            .comment()
            .map(|text| format!("  / {text}"))
            .unwrap_or_default();
        // `origin()` says where the keyword was reached from: the image itself, the file root,
        // or a reference. That is reported rather than flattened away.
        println!(
            "  {:<10} = {:<28} {:?}{comment}",
            keyword.name(),
            keyword.value(),
            keyword.origin(),
        );
    }
    print_elided(keywords.len());

    // XISF properties are typed, unlike FITS keywords which are all text.
    let properties = header.properties();
    println!("properties ({}):", properties.len());
    for property in properties.iter().take(PREVIEW) {
        // `as_str()` is a projection of the stored text, not a parse: turning it into a number
        // or a timestamp means picking a grammar, and that is your step. `None` means the value
        // lives in a data block this version does not read.
        let rendered = match property.value().as_str() {
            Some(text) => format!("{text:?}"),
            None => "(in a data block this version does not read)".to_owned(),
        };
        println!("  {:<34} {rendered}", property.id());
    }
    print_elided(properties.len());
}

fn print_elided(total: usize) {
    if total > PREVIEW {
        println!("  ... {} more", total - PREVIEW);
    }
}

/// The named lookups, against both surfaces.
///
/// Lookup is exact and does **not** case-fold: FITS keyword names are upper-case by convention
/// and this crate does not repair a file that disagrees. Both surfaces answer the same shape of
/// question, and both take the **first** match in file order — a name may repeat, and every
/// occurrence stays reachable through `keywords()`/`properties()`.
fn look_up(header: &Header, wanted: &[String]) {
    for name in wanted {
        if let Some(keyword) = header.keyword(name) {
            println!("{name} = {}", keyword.value());
        } else if let Some(property) = header.property(name) {
            println!(
                "{name} = {}",
                property.value().as_str().unwrap_or("(unavailable)")
            );
        } else {
            println!("{name} is not present");
        }
    }
}
