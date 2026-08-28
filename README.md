# astroframe

[![crates.io](https://img.shields.io/crates/v/astroframe.svg)](https://crates.io/crates/astroframe) [![docs.rs](https://img.shields.io/docsrs/astroframe?label=docs.rs)](https://docs.rs/astroframe) [![CI](https://img.shields.io/github/actions/workflow/status/macourteau/astroframe-rs/ci.yml?branch=main&label=CI)](https://github.com/macourteau/astroframe-rs/actions/workflows/ci.yml) ![MSRV](https://img.shields.io/crates/msrv/astroframe.svg)

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

fn survey(path: &str) -> astroframe::Result<()> {
    let mut reader = Reader::open(path)?;

    // A file holds one or more image *positions*: FITS calls them HDUs, XISF calls them
    // <Image> elements. `next_image` walks both and returns false at the end.
    while reader.next_image()? {
        // Past a successful advance a header always exists, so this is a `Result` rather
        // than an `Option`.
        let header = reader.current_header()?;

        // A position this version will not decode says so, in a class and a sentence, rather
        // than by erroring — and the rest of the file still walks.
        if let Some(decline) = header.decline_reason() {
            println!("declined: {decline}");
            continue;
        }

        // The three axes move as a unit, so they are read as one value.
        if let Some(g) = header.geometry() {
            println!("{} x {} x {}", g.width, g.height, g.channels);
        }

        // Where the normalization range came from is reported, never guessed at.
        match header.bounds() {
            Bounds::Declared(r) => println!("bounds {}..{} (declared by the file)", r.lo(), r.hi()),
            Bounds::FormatDefault(r) => println!("bounds {}..{} (format default)", r.lo(), r.hi()),
            other => println!("bounds {other:?}"),
        }
    }

    Ok(())
}
```

[`examples/`](examples/) walks the rest as a ladder: header-only, whole-image decode, native
samples, metadata, streaming, untrusted input, and multi-channel bounds.

## What it reads, and what has been tested

Every row below is graded against the code and the test suite rather than against the
specifications, and the **Tested** column names the evidence, because the tiers are different
promises:

- **CI** — covered by fixtures built byte by byte in the test source. These run on every push.
- **corpus** — covered by a collection of frames that lives outside the repository, is reached
  through `ASTROFRAME_CORPUS`, and backs `#[ignore]`d tests. Most of it is production-application
  output; where a production writer cannot emit an axis at all, the container is assembled from
  streams that writer's own compressor produced. It is deliberately never a CI dependency, and
  building or testing the crate needs none of it.
- **untested** — reachable and believed correct, exercised by neither. No row carries it; the
  tier is defined because a row that earns it must be able to say so.

A **decline** is not an error: a position this version does not read reports a class and a
reason, and the walk continues. The class is `Unsupported` where the file is valid and uses
something out of scope, and `Malformed` where the file contradicts the format.

### Containers

| Container | Tested |
| --- | --- |
| FITS primary HDU and `IMAGE` extensions, `NAXIS` 2 or 3, including the `INHERIT` chain for `BSCALE`, `BZERO` and `ROWORDER` | CI + corpus |
| XISF 1.0 monolithic units | CI + corpus |

What the walk meets and does not read:

| Not read | Outcome | Tested |
| --- | --- | --- |
| FITS tile-compressed images — `ZIMAGE = T` on a `BINTABLE`, as `fpack` writes | declined, `Unsupported` | CI + corpus |
| FITS random groups — `GROUPS = T` | declined, `Unsupported` | CI |
| FITS `NAXIS = 1`, `NAXIS > 3`, and any zero-length axis | declined, `Unsupported` | CI |
| FITS `TABLE` and `BINTABLE` extensions carrying no `ZIMAGE` | stepped over, so not a declined position either | CI |
| XISF external block locations, which is what a distributed unit's `<Image>` carries | declined, `Malformed` — §10.2 forbids one in a monolithic unit | CI |
| XISF `Complex32` and `Complex64` | declined, `Unsupported` | CI |
| XISF `colorSpace="CIELab"` | declined, `Unsupported` | CI |

A `NAXIS = 0` primary is not a decline at all: it is the ordinary shape of a multi-extension
file, and the walk advances past it to the extensions.

### Sample formats

`Header::sample_format` reports the **storage** type the container declares, and native samples
come back in exactly that type. FITS `BITPIX` 16, 32 and 64 store *signed* integers, and XISF
1.0 defines no signed sample format at all — so `I16`, `I32` and `I64` reach the API through
FITS alone, and there is no `I8` because no source can produce one.

| FITS `BITPIX` | `SampleFormat` | Tested |
| --- | --- | --- |
| `8` | `U8` | CI + corpus |
| `16` | `I16` | CI + corpus |
| `32` | `I32` | CI + corpus |
| `64` | `I64` | CI + corpus |
| `-32` | `F32` | CI + corpus |
| `-64` | `F64` | CI + corpus |

Every one of the six is compared sample for sample against an independent FITS decoder on each
push. A `BITPIX` outside the set is `Malformed`, and the message names the set.

| XISF `sampleFormat` | `SampleFormat` | Tested |
| --- | --- | --- |
| `UInt8` | `U8` | CI + corpus |
| `UInt16` | `U16` | CI + corpus |
| `UInt32` | `U32` | corpus |
| `UInt64` | `U64` | corpus |
| `Float32` | `F32` | CI + corpus |
| `Float64` | `F64` | corpus |

`UInt64` is the one row the fixtures do not reach: they exercise 64-bit arithmetic through the
normalization primitive and through the geometry caps, never through a container carrying
64-bit pixels. Its evidence is 19 corpus frames spanning the four codecs with and without byte
shuffling, the three block checksums, subblocked and unsubblocked blocks, both byte orders,
both pixel storages, and a declared `bounds` against the format default — every one of them
walked by the corpus sweep, native samples included. Most carry a value set chosen to stress
the arithmetic rather than to look like an image: `2⁵³ ± 1`, `2⁶³`, an exact `f32` midpoint and
its double-rounding neighbour, and `2⁶⁴ − 1`.

### FITS integers and the unsigned convention

`BITPIX` alone decides the reported `SampleFormat`, so a frame carrying the unsigned convention
reports the signed storage type it is stored in — `BITPIX = 16` is `I16` whether `BZERO` is
32768 or 0 — and native samples are the stored signed integers, unshifted. The convention shows
up in two other places instead. `Header::scaling` reports `BSCALE` and `BZERO` verbatim, and
`Header::bounds` supplies a format default only where the physical values provably occupy
`[0, 2ⁿ − 1]`. Normalization then applies `BSCALE × raw + BZERO` before the range map, so a
stored `-32768` reaches `0.0` and `32767` reaches `1.0`.

| `BITPIX` | `BSCALE`, `BZERO` | `Header::bounds` | Normalized output |
| --- | --- | --- | --- |
| `8` | `1`, `0` | `FormatDefault(0, 255)` | yes |
| `16` | `1`, `32768` | `FormatDefault(0, 65535)` | yes |
| `32` | `1`, `2147483648` | `FormatDefault(0, 2³² − 1)` | yes |
| `64` | `1`, `2⁶³` | `FormatDefault(0, 2⁶⁴)` | yes |
| any integer `BITPIX` | any other `BSCALE`, `BZERO` pairing | `Unavailable(NoFormatDefault)` | only through `set_bounds` |
| `-32`, `-64` | any | `Unavailable(NoFormatDefault)` | only through `set_bounds` |

Sixty-four bits is the one width where the reported `hi` is not the literal `2ⁿ − 1`, and it is
a property of the type rather than a slip: `Bounds` carries `f64` endpoints, `2⁶⁴ − 1` has no
`f64`, and the nearest one is `2⁶⁴`. The same holds for XISF `UInt64`. Nothing downstream
drifts, because both endpoints are powers of two: `k = 1.0f32 / ((hi − lo) as f32)` is exactly
`2⁻⁶⁴`, and `2⁶⁴ − 1` still normalizes to exactly `1.0`.

Any other pairing is refused rather than normalized: a genuinely signed frame would have half
its levels saturate to black and a rescaled frame would normalize to a sliver near zero, and
both would *look* like images. FITS defines no representable range for floats either, and
`DATAMIN`/`DATAMAX` are reported as ordinary keywords rather than consumed — they describe the
range the data occupies, not the range it is displayed against. XISF is the other way round:
an integer image takes §8.5.5's `[0, 2ⁿ − 1]` default, and §11.5.1 makes `bounds` mandatory on
a floating-point image, so one without it has no normalized output either.

### XISF block codecs

| `compression` | Tested |
| --- | --- |
| `zlib`, `zlib+sh` | CI + corpus |
| `lz4`, `lz4+sh` | CI + corpus |
| `lz4hc`, `lz4hc+sh` | CI + corpus |
| `zstd`, `zstd+sh` | CI + corpus |
| `subblocks`, beside any of the above | CI + corpus |
| any other codec name | declined, `Unsupported` (CI) |

`zstd` appears nowhere in XISF 1.0 and is read because production writers emit it. `+sh` is the
byte-shuffling modifier, and the unshuffle is computed per byte on the way out rather than by
materializing an unshuffled copy of the block.

The corpus half of the `subblocks` row is 79 frames carrying a genuine `subblocks` attribute;
see § Where the corpus evidence comes from for what they span and where the streams inside them
were produced.

### XISF block checksums

The `checksum` feature is on by default. §10.5 makes SHA-1 the only algorithm a decoder
claiming checksum support must implement; all five are read, each under both of the spellings
Table 9 gives it. A block that declares a checksum is verified before anything decompresses it,
and with the feature off such a block is declined rather than trusted.

| `checksum` algorithm | Tested |
| --- | --- |
| `sha-1`, `sha1` | CI + corpus |
| `sha-256`, `sha256` | CI + corpus |
| `sha-512`, `sha512` | CI + corpus |
| `sha3-256` | CI |
| `sha3-512` | CI |
| any other algorithm name | declined, `Unsupported` (CI) |

### XISF block locations

| `location` | Outcome | Tested |
| --- | --- | --- |
| `attachment:<position>:<size>`, and the `attached:` spelling | read | CI + corpus |
| `embedded`, with a child `<Data>` element in `base64` | read | CI + corpus |
| `embedded`, with a child `<Data>` element in `hex` | read | CI |
| `inline:<encoding>` on an `<Image>` | declined, `Malformed` — §11.5 forbids it | CI |
| an external file | declined, `Malformed` — §10.2 forbids it in a monolithic unit | CI |

### Streaming granularity

`Header::granularity` reports **how much of the input the decoder must hold before it can
produce any sample**, per position and before a pixel is read. Each property of a block imposes
a floor and the granularity is the worst of them, so `subblocks` lowers nothing once shuffling
or a checksum spans the whole block. The row-by-row table is in
[the crate documentation](https://docs.rs/astroframe), and both halves of every row — the
reported granularity and the peak memory it promises — are graded on each push.

### Where the corpus evidence comes from

The corpus is a local collection held outside the repository and never committed: it carries
observatory coordinates at full precision, and not all of the frames are the maintainer's own.
Its XISF half is a complete matrix of 5 sample formats × 9 codec and shuffling combinations ×
4 checksum settings — 1080 frames — written by PixInsight from the FITS frames they came from,
which is what makes the FITS decode an independent oracle for the XISF one: 41.8 billion pixels
compared across the pair, three of the five sample formats bit-exact and the other two inside a
derived tolerance. Its tile-compressed half is 120 `fpack`-written frames, every one of which
must decline cleanly rather than merely fail. No CI lane may set `ASTROFRAME_CORPUS`, and the
build-and-test job fails if it is set.

The sweep enumerates the corpus root recursively rather than a list of family names, so an axis
added to it is graded from the day it lands rather than the day someone remembers to widen a
list. A run walks 1542 frames: 1422 decode, 120 decline as tile-compressed, none fail. Two
exclusions are stated rather than silent — files broken on purpose, which belong to the tests
that assert the rejection they expect, and 8 images whose blocks inflate to between 2.1 and
4.5 GB, which are opened and header-checked but not decoded, because decoding them would cost
more than the rest of the corpus combined.

Two axes are out of reach of an ordinary export, and are covered by frames whose containers are
assembled but whose compressed streams are not: 79 subblocked frames across 8 codec and
shuffling combinations and subblock counts from 2 to 192, and 19 XISF `UInt64` frames beside
3 FITS `BITPIX = 64` frames. PixInsight splits a block only when it exceeds the codec's own
input ceiling — gigabytes, and no hint asks for one below that — and it refuses a 64-bit
integer sample format outright. So every compressed stream among these comes from its
compressor and is round-tripped byte for byte through its decompressor, and only the container
around it is built here. The frames that do straddle the split threshold are the 8 the sweep
reads header-only.

Separately, a validation run of 0.1.1 over 163 public-archive frames — ESO, MAST, IRSA, SDSS
and SkyView — walked 512 image positions: 410 decoded, 102 declined with a stated reason, none
failed, and every decoded position was byte-identical to an independent decoder over the raw
sample bytes. That run is a maintainer's record rather than a test in this repository.

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
