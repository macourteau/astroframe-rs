//! FITS block and card parsing, value lexing, and the character-set rule.
#![no_main]

use astroframe_fuzz::{assert_within, bound, start_counting, walk_headers};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Give the sniffer a reason to take the FITS path, so the fuzzer spends its budget on
    // card parsing rather than on rediscovering the signature.
    let mut input = Vec::with_capacity(data.len() + 8);
    input.extend_from_slice(b"SIMPLE  ");
    input.extend_from_slice(data);

    let run = start_counting();
    walk_headers(&input);
    assert_within(&run, bound(input.len()), "fits_header", input.len());
});
