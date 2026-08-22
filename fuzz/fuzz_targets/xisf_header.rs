//! The XISF preamble, the XML header, the XML guards and attribute parsing.
#![no_main]

use astroframe_fuzz::{assert_within, bound, start_counting, walk_headers};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The signature and a declared header length covering whatever follows, so the fuzzer
    // reaches the XML rather than being turned away at the preamble.
    let mut input = Vec::with_capacity(data.len() + 16);
    input.extend_from_slice(b"XISF0100");
    input.extend_from_slice(&(data.len() as u32).to_le_bytes());
    input.extend_from_slice(&[0u8; 4]);
    input.extend_from_slice(data);

    let run = start_counting();
    walk_headers(&input);
    assert_within(&run, bound(input.len()), "xisf_header", input.len());
});
