//! The allocating `read_image()` path, which `decode_any` cannot reach.
//!
//! This is the target that exercises geometry-driven allocation, so its oracle carries the
//! geometry-implied size on top of the general bound — the one place the extra room belongs.
#![no_main]

use astroframe_fuzz::{accumulate_implied, assert_within, bound, options, start_counting};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let run = start_counting();
    let mut implied: usize = 0;
    if let Ok(mut reader) = options(data).seekable(std::io::Cursor::new(data)) {
        while matches!(reader.next_image(), Ok(true)) {
            if let Ok(h) = reader.current_header()
                && let (Some(w), Some(ht), Some(c), Some(f)) =
                    (h.width(), h.height(), h.channels(), h.sample_format())
            {
                implied = accumulate_implied(implied, w, ht, c, f.bytes());
            }
            let _ = reader.read_image();
        }
    }
    assert_within(
        &run,
        bound(data.len()).saturating_add(implied),
        "decode_alloc",
        data.len(),
    );
});
