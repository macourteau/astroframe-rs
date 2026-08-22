//! Criterion *A decompression bomb is refused without materializing*.
//!
//! A block declaring a small geometry-implied size but inflating to many megabytes is
//! rejected, **and the allocation never happens**. Run for zlib, LZ4 and the zstd declared
//! window — the three shapes § Hardening distinguishes, which fail in three different places:
//!
//! | Codec | Where the bomb dies |
//! | --- | --- |
//! | `zlib` | The stream inflates to exactly the geometry-implied length and is then required to be at end of stream, so the remaining megabytes are refused after a few dozen bytes |
//! | `lz4` | A bare block decompresses only as a whole, into a destination sized from the **geometry** — the payload has nowhere to go |
//! | `zstd` | The frame header's declared window is checked **before** the window is allocated, which is the one place a declared number would otherwise become an allocation |
//!
//! Their error classes are graded case-for-case in `tests/adversarial.rs`, alongside the rest
//! of the ported corpus. What this file adds is the measurement: the corresponding case in the
//! original suite **measures no allocation** despite its name, and this is where the assertion
//! its name implies actually lives.
//!
//! ## The trap this file is arranged around
//!
//! A `#[global_allocator]` counts the **whole process**, and the test harness runs `#[test]`s
//! on parallel threads — so a sibling test building a fixture lands in the counter a measuring
//! test just reset. In `tests/peak_memory.rs` that produced a false 60 MB reading against a
//! model of 9 MB and looked exactly like a decoder defect. So: **every measurement lives in
//! one `#[test]`, and every fixture is built before the first `reset()`.** Nothing else in
//! this binary allocates while a measurement is running.

// No crate-level `forbid(unsafe_code)` here, and only here: `#[global_allocator]` requires an
// `unsafe impl GlobalAlloc`, and the counting allocator this file links is that one carve-out.
// It lives in `tests/common/alloc.rs` with its own `#[allow(unsafe_code)]` and its safety
// argument; the library itself stays `#![forbid(unsafe_code)]`.

#[path = "common/alloc.rs"]
mod alloc;
mod common;

use std::io::Cursor;

use astroframe::{Error, Limits, Reader};

use common::xisf::{Unit, le_u16, lz4, repeating_u16, zlib};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// What each bomb would cost if it were materialized. The geometry implies 32 bytes in every
/// case, so the ratio between this and the allowance below is the whole point.
const INFLATED: usize = 8 << 20;

/// What a refusal is allowed to cost, stated as a fraction of the payload so the relationship
/// is the assertion rather than a number someone once measured.
///
/// A refusal is not free and is not claimed to be: the decoder reads the stored block, holds a
/// fixed 64 KiB header-read buffer, and an inflate decoder brings its own window. Measured, the
/// three refusals cost about 160 KB, 100 KB and 70 KB, against a 70 KB floor that an honest
/// decode of the same geometry also pays — so this leaves roughly 3x headroom over the worst
/// of them while a decode that materialized the payload would miss by 16x.
const ALLOWED: usize = INFLATED / 16;

fn kind(e: &Error) -> &'static str {
    match e {
        Error::Io(_) => "Io",
        Error::Malformed(_) => "Malformed",
        Error::Unsupported(_) => "Unsupported",
        Error::ChecksumMismatch(_) => "ChecksumMismatch",
        Error::LimitExceeded(_) => "LimitExceeded",
        Error::InvalidRequest(_) => "InvalidRequest",
        other => panic!("a variant this suite does not know: {other:?}"),
    }
}

fn attach_image(attrs: &str, block: Vec<u8>) -> Vec<u8> {
    Unit::new()
        .attached(&format!("<Image {attrs} {{loc}}/>"), block)
        .build()
}

/// The one measuring test. Every fixture is built first, then each decode is attempted with
/// the counter reset immediately before it — see the module documentation for why this is one
/// test and not four.
#[test]
fn a_decompression_bomb_is_refused_without_materializing() {
    // ---- fixtures, all built before any measurement begins ----

    // The geometry implies 4 x 4 x 2 = 32 bytes; the stream inflates to 8 MiB.
    let zlib_bomb = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="zlib:32""#,
        zlib(&vec![0u8; INFLATED]),
    );
    let lz4_bomb = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="lz4:32""#,
        lz4(&vec![0u8; INFLATED]),
    );
    // A zstd frame header declaring a 1 GiB window, with nothing behind it: a zstd decoder
    // allocates the declared window before producing a byte, so the refusal has to come from
    // the header. RFC 8878 §3.1.1 — magic, a zero Frame_Header_Descriptor, and a
    // Window_Descriptor of `Exponent << 3 | Mantissa` for exponent 20, i.e. window log 30.
    let zstd_bomb = attach_image(
        r#"geometry="4:4:1" sampleFormat="UInt16" compression="zstd:32""#,
        vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 20 << 3],
    );

    // A control: the same geometry, honestly stored. It proves the harness measures something
    // — a decode that really does produce 32 bytes of pixels costs a bounded amount too, so a
    // measurement of zero everywhere would be a broken counter rather than a tight decoder.
    let levels = repeating_u16(16);
    let honest = attach_image(r#"geometry="4:4:1" sampleFormat="UInt16""#, le_u16(&levels));

    // Warm every allocation the first decode would otherwise pay for on behalf of the rest:
    // lazily initialized statics inside the decoder or its dependencies would land in whichever
    // measurement ran first and look like that codec's cost.
    let mut warm = Reader::seekable(Cursor::new(&honest[..])).expect("construct the control");
    assert!(warm.next_image().expect("advance"));
    let _ = warm.read_image().expect("the control decodes");
    drop(warm);

    // ---- measurements ----

    measure("zlib", &zlib_bomb, "Malformed", "inflates past");
    measure("lz4", &lz4_bomb, "Malformed", "does not decompress");
    measure(
        "zstd declared window",
        &zstd_bomb,
        "LimitExceeded",
        "window",
    );

    // The control, measured the same way: a real decode of this geometry is also cheap, so the
    // three figures above are not simply "nothing ran".
    alloc::reset();
    let mut reader = Reader::seekable(Cursor::new(&honest[..])).expect("construct");
    assert!(reader.next_image().expect("advance"));
    let image = reader.read_image().expect("the control decodes");
    let used = alloc::total();
    assert!(
        used <= ALLOWED,
        "the control decode allocated {used} bytes, above the {ALLOWED} allowance"
    );
    for (i, (got, level)) in image.samples().iter().zip(&levels).enumerate() {
        let want = *level as f32 * (1.0f32 / 65535.0f32);
        assert_eq!(got.to_bits(), want.to_bits(), "control: sample {i}");
    }
}

/// Attempt one decode with the counter reset immediately before it, and assert both halves:
/// the refusal's class **and** that the payload was never materialized.
fn measure(case: &str, bytes: &[u8], want_kind: &'static str, fragment: &str) {
    alloc::reset();
    let outcome = decode(bytes);
    let used = alloc::total();

    let e = match outcome {
        Ok(samples) => panic!("{case}: the bomb decoded to {samples} samples"),
        Err(e) => e,
    };
    assert_eq!(kind(&e), want_kind, "{case}: {e}");
    let text = e.to_string();
    assert!(
        text.contains(fragment),
        "{case}: the error should name {fragment:?}, got {text:?}"
    );
    assert!(
        used <= ALLOWED,
        "{case}: refusing the bomb allocated {used} bytes, above the {ALLOWED} allowance. \
         Materializing the {INFLATED}-byte payload would land here — which is the whole \
         difference between a bomb that is caught and one that is structurally impossible."
    );
}

/// Kept out of [`measure`] so the `Image` — the only thing here that could hold a large buffer
/// — is dropped before the counter is read, and so a successful decode reports its sample
/// count rather than being silently discarded.
fn decode(bytes: &[u8]) -> Result<usize, Error> {
    let mut reader = Reader::seekable_with_limits(Cursor::new(bytes), Limits::default())?;
    if !reader.next_image()? {
        panic!("a bomb fixture must reach an image position");
    }
    reader.read_image().map(|image| image.samples().len())
}
