//! Decompression, unshuffling, subblock splitting and checksum verification — where the size
//! arithmetic and the allocation guards live.
#![no_main]

use std::ops::ControlFlow;

use astroframe::Samples;
use astroframe_fuzz::{assert_within, bound, options, start_counting};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&[u8], &[u8])| {
    let (header, body) = input;
    let mut unit = Vec::with_capacity(header.len() + body.len() + 16);
    unit.extend_from_slice(b"XISF0100");
    unit.extend_from_slice(&(header.len() as u32).to_le_bytes());
    unit.extend_from_slice(&[0u8; 4]);
    unit.extend_from_slice(header);
    unit.extend_from_slice(body);

    let run = start_counting();
    if let Ok(mut reader) = options(&unit).seekable(std::io::Cursor::new(&unit)) {
        // One native destination, built for the first image that declares a geometry and held
        // for the rest of the run. `Samples` is typed by sample format, so no one buffer serves
        // every image, and replacing it on each change of format charged a fresh 2 MiB per
        // image to a run of distinct images alternating `Float64` and `UInt64` — 16.9 MB from
        // 1068 bytes at eight images, the harness's allocation rather than the library's.
        let mut samples: Option<Samples> = None;
        while matches!(reader.next_image(), Ok(true)) {
            let Ok(h) = reader.current_header() else {
                break;
            };
            let (Some(w), Some(ht), Some(c), Some(f)) =
                (h.width(), h.height(), h.channels(), h.sample_format())
            else {
                continue;
            };
            // Sized from the parsed header and bounded by the caps, never from a fixed
            // capacity: a fixed one would make almost every fuzzer-generated geometry an
            // InvalidRequest before a pixel byte was touched.
            let len = (w as u64 * ht as u64 * c as u64).min(1 << 18) as usize;
            let held = samples.get_or_insert_with(|| Samples::zeroed(f, len));
            if held.format() == f && held.len() == len {
                // The common case — one image repeated by `<Reference>` — and the one that
                // keeps `read_samples_into`'s checks of the buffer and its decoded-output cap
                // at the native width under the fuzzer.
                let _ = reader.read_samples_into(held);
            } else {
                // Any other image is read through the chunk cursor `read_samples_into` is
                // built on, which needs no destination.
                let _ = reader.for_each_chunk(|_| ControlFlow::Continue(()));
            }
        }
    }
    assert_within(&run, bound(unit.len()), "xisf_block", unit.len());
});
