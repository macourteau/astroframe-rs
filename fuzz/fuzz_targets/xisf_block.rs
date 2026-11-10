//! Decompression, unshuffling, subblock splitting and checksum verification — where the size
//! arithmetic and the allocation guards live.
#![no_main]

use std::ops::ControlFlow;

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
        while matches!(reader.next_image(), Ok(true)) {
            if reader.current_header().is_err() {
                break;
            }
            // Native samples through the chunk cursor, which every native read is built on, so
            // the target holds no destination of its own. A `Samples` destination is typed by
            // sample format, so no one buffer can serve every image: a run of distinct images
            // alternating `Float64` and `UInt64` charged a fresh 2 MiB per image — 16.9 MB
            // from 1068 bytes at eight images — which is the harness's allocation, not the
            // library's. What is given up is `read_samples_into`'s checks of the buffer it is
            // handed, and its decoded-output cap at the native width, which `decode_any` reaches
            // at `f32`'s; none of them decodes anything.
            let _ = reader.for_each_chunk(|_| ControlFlow::Continue(()));
        }
    }
    assert_within(&run, bound(unit.len()), "xisf_block", unit.len());
});
