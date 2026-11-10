//! Format sniffing through to a full decode.
#![no_main]

use astroframe_fuzz::{assert_within, bound, options, start_counting};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let run = start_counting();
    if let Ok(mut reader) = options(data).seekable(std::io::Cursor::new(data)) {
        // One destination held across images, as a caller decoding a sequence holds one. A
        // fresh one per image charges the harness's own buffer once per occurrence, and
        // `<Reference>` makes an occurrence cost twenty bytes of input.
        let mut dst: Vec<f32> = Vec::new();
        while matches!(reader.next_image(), Ok(true)) {
            let Ok(h) = reader.current_header() else {
                break;
            };
            let (Some(w), Some(ht), Some(c)) = (h.width(), h.height(), h.channels()) else {
                continue;
            };
            // The destination is sized **from the parsed header**, bounded by the caps.
            let len = (w as u64 * ht as u64 * c as u64).min(1 << 18) as usize;
            dst.clear();
            dst.resize(len, 0.0);
            let _ = reader.read_image_into(&mut dst);
        }
    }
    assert_within(&run, bound(data.len()), "decode_any", data.len());
});
