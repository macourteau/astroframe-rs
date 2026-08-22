//! Format sniffing through to a full decode.
#![no_main]

use astroframe::Reader;
use astroframe_fuzz::{assert_within, bound, limits, start_counting};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let run = start_counting();
    if let Ok(mut reader) = Reader::seekable_with_limits(std::io::Cursor::new(data), limits()) {
        while matches!(reader.next_image(), Ok(true)) {
            let Some(h) = reader.header() else { break };
            let (Some(w), Some(ht), Some(c)) = (h.width(), h.height(), h.channels()) else {
                continue;
            };
            // The destination is sized **from the parsed header**, bounded by the caps.
            let len = (w as u64 * ht as u64 * c as u64).min(1 << 18) as usize;
            let mut dst = vec![0.0f32; len];
            let _ = reader.read_image_into(&mut dst);
        }
    }
    assert_within(&run, bound(data.len()), "decode_any", data.len());
});
