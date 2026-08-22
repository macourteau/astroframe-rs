//! Decompression, unshuffling, subblock splitting and checksum verification — where the size
//! arithmetic and the allocation guards live.
#![no_main]

use astroframe::{Reader, Samples};
use astroframe_fuzz::{assert_within, bound, limits, start_counting};
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
    if let Ok(mut reader) = Reader::seekable_with_limits(std::io::Cursor::new(&unit), limits()) {
        while matches!(reader.next_image(), Ok(true)) {
            let Some(h) = reader.header() else { break };
            let (Some(w), Some(ht), Some(c), Some(f)) =
                (h.width(), h.height(), h.channels(), h.sample_format())
            else {
                continue;
            };
            // Sized from the parsed header and bounded by the caps, never from a fixed
            // capacity: a fixed one would make almost every fuzzer-generated geometry an
            // InvalidRequest before a pixel byte was touched.
            let len = (w as u64 * ht as u64 * c as u64).min(1 << 18) as usize;
            let mut samples = Samples::zeroed(f, len);
            let _ = reader.read_samples_into(&mut samples);
        }
    }
    assert_within(&run, bound(unit.len()), "xisf_block", unit.len());
});
