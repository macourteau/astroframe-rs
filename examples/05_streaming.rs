//! Tier 3 — chunked delivery, and a forward-only source.
//!
//! Two things worth knowing before you reach for this:
//!
//! * **`granularity()` tells you whether streaming buys anything, before you decode.** It
//!   reports how much of the input the decoder must hold before it can produce *any* sample.
//!   `Rows` is the win; `WholeImage` means the frame is materialized either way and tier 2 is
//!   simpler. A compressed single-block XISF image is `WholeImage`, and the API says so rather
//!   than letting you find out from a memory graph.
//! * **`Reader::sequential` reads a `Read` with no `Seek`** — a pipe, a socket, a decompressing
//!   wrapper. Some things are `Unsupported` there that work on a file, because they need to
//!   move the cursor backwards; that is a stated refusal, not a silent difference.
//!
//! ```text
//! cargo run --release --example 05_streaming -- frame.fits
//! cat frame.fits | cargo run --release --example 05_streaming
//! ```

use astroframe::{Granularity, Reader, SampleSlice};
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

fn run<S: astroframe::Source>(mut reader: Reader<S>) -> astroframe::Result<()> {
    while reader.next_image()? {
        let header = reader.header().expect("an advanced reader has a header");
        if header.decline_reason().is_some() {
            continue;
        }
        let granularity = header.granularity();
        match granularity {
            Granularity::Rows => println!("streams by rows — chunking is worth it"),
            Granularity::Block { subblocks } => {
                println!("streams in {subblocks} blocks — partial, but better than nothing");
            }
            other => println!("{other:?} — the whole frame is held either way; tier 2 is simpler"),
        }

        // Accumulate without ever holding the frame twice: each chunk is borrowed from the
        // decoder's own buffer and is valid only inside the callback.
        let mut chunks = 0usize;
        let mut samples = 0usize;
        let mut peak_chunk = 0usize;
        reader.for_each_chunk(|chunk| {
            chunks += 1;
            // `range()` is in **destination** coordinates — offsets into the buffer you would
            // be filling — so assembling one is a copy at the stated offset with no
            // recalculation. `channel()` is the **file's** channel index. The two numbering
            // schemes diverge under `select_channel` and neither derives from the other.
            let n = chunk.range().len();
            samples += n;
            peak_chunk = peak_chunk.max(n);
            let _ = chunk.channel();
            // Samples arrive in the file's own type, not normalized.
            if let SampleSlice::U16(_) = chunk.samples() {
                // ... a 16-bit path would go here
            }
            ControlFlow::Continue(())
        })?;

        println!("  {chunks} chunks, {samples} samples, largest chunk {peak_chunk} samples");
    }
    Ok(())
}
