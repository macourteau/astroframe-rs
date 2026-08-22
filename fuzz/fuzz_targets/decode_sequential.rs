//! The forward-only source: read-and-discard skipping, the backwards-block `Unsupported`
//! rule, thumbnail skipping, and the no-known-length branch of every size check.
//!
//! Every other target hands the reader a `Cursor`, which is seekable, so this path — the
//! shape a forward-only, non-seekable caller needs, and the one the FITS and stored-block
//! caps exist for — would
//! otherwise be entirely unfuzzed.
#![no_main]

use astroframe::Reader;
use astroframe_fuzz::{assert_within, bound, limits, start_counting};
use libfuzzer_sys::fuzz_target;

/// A `Read` with no `Seek`, so the reader takes the sequential source.
struct Forward<'a> {
    data: &'a [u8],
    at: usize,
}

impl std::io::Read for Forward<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.data.len() - self.at).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fuzz_target!(|data: &[u8]| {
    let run = start_counting();
    let source = Forward { data, at: 0 };
    if let Ok(mut reader) = Reader::sequential_with_limits(source, limits()) {
        while matches!(reader.next_image(), Ok(true)) {
            let Some(h) = reader.header() else { break };
            let (Some(w), Some(ht), Some(c)) = (h.width(), h.height(), h.channels()) else {
                continue;
            };
            let len = (w as u64 * ht as u64 * c as u64).min(1 << 18) as usize;
            let mut dst = vec![0.0f32; len];
            let _ = reader.read_image_into(&mut dst);
        }
    }
    assert_within(&run, bound(data.len()), "decode_sequential", data.len());
});
