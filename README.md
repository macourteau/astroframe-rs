# astroframe

A decode-only Rust library for **FITS** and **XISF** astronomical image frames.

It reads a frame and produces two things: the header metadata a tool needs — geometry,
exposure, gain, pixel scale, pointing, timestamps — and the pixels themselves, either in the
file's own sample type or normalized to `f32` in `[0,1]`, row-major and channel-planar.
Nothing in it writes a frame.

It is pure Rust end to end: no C library, no `-sys` crate, and no build-time toolchain
beyond `cargo`. The compression and hashing dependencies are Rust implementations, and the
crate itself sets `#![forbid(unsafe_code)]`.

```toml
[dependencies]
astroframe = "0.1"
```

## Reading a header without decoding a pixel

Constructing a `Reader` and reading a header reads no pixel byte, which for a metadata sweep
over a night's frames is the whole job — and it is the same code for both formats.

```rust
use astroframe::{Bounds, Reader};

let mut reader = Reader::open("frame.fits")?;

// A file holds one or more image *positions*: FITS calls them HDUs, XISF calls them
// <Image> elements. `next_image` walks both and returns false at the end.
while reader.next_image()? {
    let header = reader.header().expect("an advanced reader has a header");

    // A position this version will not decode says so, in a class and a sentence, rather
    // than by erroring — and the rest of the file still walks.
    if let Some(decline) = header.decline_reason() {
        println!("declined ({:?}): {}", decline.class(), decline.reason());
        continue;
    }

    if let (Some(w), Some(h), Some(c)) = (header.width(), header.height(), header.channels()) {
        println!("{w} x {h} x {c}");
    }

    // Where the normalization range came from is reported, never guessed at.
    match header.bounds() {
        Bounds::Declared(lo, hi) => println!("bounds {lo}..{hi} (declared by the file)"),
        Bounds::FormatDefault(lo, hi) => println!("bounds {lo}..{hi} (format default)"),
        other => println!("bounds {other:?}"),
    }
}
# Ok::<(), astroframe::Error>(())
```

[`examples/`](examples/) walks the rest as a ladder: header-only, whole-image decode, native
samples, metadata, streaming, untrusted input, and multi-channel bounds.

## Report, don't interpret

`astroframe` reports what the file says. It applies exactly one transformation — the
sample-to-`[0,1]` normalization the format specifications define — and it applies no other.
A `ROWORDER = 'BOTTOM-UP'` frame is reported as bottom-up and delivered in stored order; an
XISF `orientation` is reported and applied to nothing; `PEDESTAL` and XISF `offset` are
reported and subtracted from nothing. Policy belongs to the consumer.

The rule cuts the other way too: a frame it cannot decode under its documented rules is an
error, never a best guess. And **a decline is not an error** — a position this version does
not handle reports a class and a reason, and the walk continues.

## Normalization is pinned bit-for-bit

One primitive, shared by every container, so the two formats cannot drift apart. It is
deliberately **not** the idiomatic `x as f32 / 65535.0` — the two forms disagree by one ULP on
0.78% of 16-bit levels. **The decoded bits are part of the public API**, so that difference is
a breaking change rather than a cleanup. See
[`docs/intentional-patterns.md`](https://github.com/macourteau/astroframe-rs/blob/main/docs/intentional-patterns.md)
before touching it.

## Features

| Feature | Default | What it adds |
| --- | --- | --- |
| `fits` | yes | The FITS decoder |
| `xisf` | yes | The XISF decoder, and with it `quick-xml`, `lz4_flex`, `flate2`, `ruzstd` and `base64` |
| `checksum` | yes | Verification of XISF block checksums (`sha1`, `sha2`, `sha3`) |

The **empty** feature set is a supported configuration, not an accident: with both formats off
the crate still compiles and still exposes the header types and the normalization primitive,
and every constructor returns `Unsupported`. `checksum` without `xisf` is inert.

## Minimum supported Rust version

**1.88.** The MSRV is reported rather than targeted, and CI checks that the declared number
stays true. Raising it is a minor bump at `0.x`.

## Documentation

- [The design document](https://github.com/macourteau/astroframe-rs/blob/main/docs/design/2026-08-18-astroframe-library.md)
  — the specification this crate is graded against, and a record of why each rule is what it is.
- [`docs/implementation-decisions.md`](https://github.com/macourteau/astroframe-rs/blob/main/docs/implementation-decisions.md)
  — choices the design left to the implementer.
- [`docs/intentional-patterns.md`](https://github.com/macourteau/astroframe-rs/blob/main/docs/intentional-patterns.md)
  — code that looks wrong and is not.
- [`CONTRIBUTING.md`](https://github.com/macourteau/astroframe-rs/blob/main/CONTRIBUTING.md)
  — what to run before opening a pull request.

## Licence

`MIT OR Apache-2.0`, at your option. See [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
