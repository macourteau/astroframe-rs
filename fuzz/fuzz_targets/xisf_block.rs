//! Decompression, unshuffling, subblock splitting and checksum verification — where the size
//! arithmetic and the allocation guards live.
#![no_main]

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
        // Held across images, for the reason `decode_any` gives, and replaced only when an
        // image needs a different format or length: `<Reference>` repeats one image, which is
        // the shape reuse has to cover.
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
            if !samples
                .as_ref()
                .is_some_and(|s| s.format() == f && s.len() == len)
            {
                samples = Some(Samples::zeroed(f, len));
            }
            if let Some(samples) = samples.as_mut() {
                let _ = reader.read_samples_into(samples);
            }
        }
    }
    assert_within(&run, bound(unit.len()), "xisf_block", unit.len());
});
