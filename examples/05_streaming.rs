//! Tier 3 — chunked delivery, and a forward-only source.
//!
//! Three things worth knowing before you reach for this:
//!
//! * **`granularity()` tells you whether streaming buys anything, before you decode.** It
//!   reports how much of the input the decoder must hold before it can produce *any* sample.
//!   `Rows` is the win; `WholeImage` means the frame is materialized either way and tier 2 is
//!   simpler. A compressed single-block XISF image is `WholeImage`, and the API says so rather
//!   than letting you find out from a memory graph.
//! * **Chunks carry the file's own samples, and normalizing them is one call each.**
//!   `Reader::normalizer()` hands you the primitive tier 2 uses, built from the range actually
//!   in force; `Chunk::normalize_into` applies it. Assembling a buffer that way is
//!   bit-identical to `read_image_into` by construction, not by two paths agreeing — which is
//!   what makes tier 3 a real alternative rather than a lower-level view of the same decode.
//! * **`Reader::sequential` reads a `Read` with no `Seek`** — a pipe, a socket, a decompressing
//!   wrapper. Some things are `Unsupported` there that work on a file, because they need to
//!   move the cursor backwards; that is a stated refusal, not a silent difference.
//!
//! ```text
//! cargo run --release --example 05_streaming -- frame.fits
//! cat frame.fits | cargo run --release --example 05_streaming
//! ```

use astroframe::{Error, Granularity, Reader, Source};
use std::ops::ControlFlow;

fn main() -> astroframe::Result<()> {
    // No path: read the frame from stdin, which is the forward-only case. `BufReader` matters
    // here — the decoder issues many small reads and an unbuffered pipe makes each a syscall.
    match std::env::args().nth(1) {
        Some(path) => {
            let file = std::io::BufReader::new(std::fs::File::open(&path)?);
            run(Reader::seekable(file)?)
        }
        None => {
            eprintln!("(no path given: reading a frame from stdin, forward-only)");
            let stdin = std::io::BufReader::new(std::io::stdin().lock());
            run(Reader::sequential(stdin)?)
        }
    }
}

/// Generic over the source, which is the bound a caller writes. Whether this one can decode an
/// image a second time is the reader's own answer — `is_seekable()` — rather than something the
/// caller has to thread down from wherever the reader was built.
fn run<S: Source>(mut reader: Reader<S>) -> astroframe::Result<()> {
    while reader.next_image()? {
        let header = reader.current_header()?;
        if header.decline_reason().is_some() {
            continue;
        }
        match header.granularity() {
            Granularity::Rows => println!("streams by rows — chunking is worth it"),
            Granularity::Block { subblocks, .. } => {
                println!("streams in {subblocks} blocks — partial, but better than nothing");
            }
            other => println!("{other:?} — the whole frame is held either way; tier 2 is simpler"),
        }

        // The primitive, asked for **before** the cursor commits the reader to the pixel
        // phase, and after any `select_channel`/`set_bounds` — it describes what this reader
        // will produce. A frame the file states no range for is refused here rather than
        // normalized against an invented one; see 07_channels_and_bounds for the escape hatch.
        let normalizer = match reader.normalizer() {
            Ok(n) => n,
            Err(e @ (Error::Unsupported(_) | Error::Malformed(_))) => {
                println!("  no normalized output: {e}");
                continue;
            }
            Err(e) => return Err(e),
        };
        println!(
            "  normalizing against {}..{}",
            normalizer.range().lo(),
            normalizer.range().hi()
        );

        // Sized by the reader rather than by hand: `destination_len` is the same number
        // `read_image_into` checks a destination against, narrowed by `select_channel` if one
        // was made.
        let mut assembled = vec![0.0f32; reader.destination_len()?];

        let mut chunks = 0usize;
        let mut peak_chunk = 0usize;
        reader.for_each_chunk(|chunk| {
            chunks += 1;
            // `range()` is in **destination** coordinates — offsets into the buffer you are
            // filling — so this is a write at the stated offset with no recalculation.
            // `channel()` is the **file's** channel index. The two numbering schemes diverge
            // under `select_channel` and neither derives from the other.
            let range = chunk.range();
            peak_chunk = peak_chunk.max(range.len());
            let _ = chunk.channel();
            chunk.normalize_into(&normalizer, &mut assembled[range]);
            ControlFlow::Continue(())
        })?;

        println!(
            "  {chunks} chunks, {} samples, largest chunk {peak_chunk} samples",
            assembled.len()
        );

        // Decoding this image a second time moves the cursor backwards, so the cross-check
        // runs on a file and is skipped on a pipe. The reader answers that itself.
        if reader.is_seekable() {
            // The claim, run rather than asserted: the same image decoded whole, compared by
            // `to_bits()`. `==` would accept a sign-of-zero difference, which is exactly the
            // difference a normalization defect produces.
            let mut whole = vec![0.0f32; assembled.len()];
            reader.read_image_into(&mut whole)?;
            let same = assembled
                .iter()
                .zip(&whole)
                .all(|(a, b)| a.to_bits() == b.to_bits());
            println!("  bit-identical to read_image_into: {same}");
        }
    }
    Ok(())
}
