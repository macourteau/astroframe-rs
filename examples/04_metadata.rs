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
//! cargo run --example 04_metadata -- frame.xisf DATE-OBS EXPTIME
//! ```

use astroframe::{PropertyValue, Reader};

fn main() -> astroframe::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: 04_metadata <frame> [keyword...]");
        std::process::exit(2);
    };
    let wanted: Vec<String> = args.collect();

    let mut reader = Reader::open(&path)?;
    if !reader.next_image()? {
        println!("no image positions in {path}");
        return Ok(());
    }
    let header = reader.header().expect("an advanced reader has a header");

    if wanted.is_empty() {
        // Keywords are reported in file order, duplicates and all — a FITS header may carry
        // many `HISTORY` cards and each is its own entry.
        println!("keywords ({}):", header.keywords().len());
        for keyword in header.keywords().iter().take(20) {
            let comment = keyword.comment().unwrap_or("");
            // `origin()` says where the keyword was reached from: the image itself, the file
            // root, or a reference. That is reported rather than flattened away.
            println!(
                "  {:<10} = {:<28} {:?}{}",
                keyword.name(),
                keyword.value(),
                keyword.origin(),
                if comment.is_empty() {
                    String::new()
                } else {
                    format!("  / {comment}")
                }
            );
        }
        if header.keywords().len() > 20 {
            println!("  ... {} more", header.keywords().len() - 20);
        }

        // XISF properties are typed, unlike FITS keywords which are all text.
        println!("properties ({}):", header.properties().len());
        for property in header.properties().iter().take(20) {
            let rendered = match property.value() {
                PropertyValue::Text(t) => format!("{t:?}"),
                other => format!("{other:?}"),
            };
            println!("  {:<34} {rendered}", property.id());
        }
        if header.properties().len() > 20 {
            println!("  ... {} more", header.properties().len() - 20);
        }
    } else {
        // Lookup is exact and does **not** case-fold: FITS keyword names are upper-case by
        // convention and this crate does not repair a file that disagrees.
        for name in &wanted {
            match header.get(name) {
                Some(keyword) => println!("{name} = {}", keyword.value()),
                None => println!("{name} is not present"),
            }
        }
    }
    Ok(())
}
