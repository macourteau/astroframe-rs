# Threat model — astroframe

## What this project does and where untrusted input enters

astroframe (crates.io `astroframe`) is a pure-Rust, decode-only library for FITS and XISF
astronomical image frames. It returns header metadata (geometry, exposure, pointing, keywords, XISF
properties) and pixels, as native samples or normalized to `f32` in `[0,1]`. Callers are graders,
ingest services, calibration pipelines and viewers fed frames from telescopes, public archives,
collaborators, uploads and the web, and they use the decoded numbers as scientific data. The crate
sets `#![forbid(unsafe_code)]` (`src/lib.rs`), has no C dependency and no build script, and writes
nothing.

**Every byte of the source is untrusted.** The calling program is trusted: the path it passes to
`Reader::open`, its `DecodeOptions` and the `Limits` in them, its destination slices, and its
`set_bounds` / `select_channel` arguments. Untrusted bytes enter only through a `Reader`
(`src/reader.rs`):

- **Construction**: `Reader::open(path)`, `Reader::seekable(R: Read + Seek)` (in-memory input is
  `Reader::seekable(Cursor::new(bytes))`), `Reader::sequential(R: Read)`, and the `DecodeOptions`
  finisher of each (`open` / `seekable` / `sequential`), which applies the caller's `Limits`.
  Construction sniffs `SIMPLE` / `XISF0100` and parses the first header unit: the FITS primary
  header, or the XISF preamble and the whole XML header, which includes decoding every `embedded`
  block's Base64/Base16 text and verifying its checksum. No attached pixel byte is read.
- **Walking**: `next_image()`. For FITS it parses extension headers and skips data units; non-image
  extensions and `IMAGE` extensions with `NAXIS = 0` are stepped over inside one call, and a
  tile-compressed `BINTABLE` is a declined position. For XISF it walks `<Image>` elements and the
  root-level `<Reference>`s that resolve to one. `current_header()` exposes geometry, bounds,
  scaling, `keywords()`, `properties()`, `cfa()`, `resolution()`, `display_function()`,
  `granularity()` and `decline_reason()`.
- **Pixels**: tier 2 is `read_image_into(&mut [f32])`, `read_samples_into(&mut Samples)` and the
  allocating `read_image()` / `read_samples()`. Tier 3 is `chunks()` → `Chunks::next_chunk()` and
  `for_each_chunk`, with `Chunk::normalize_into`; tier 2 is built on tier 3. `destination_len()`,
  `normalizer()`, `select_channel()` and `set_bounds()` shape the decode.
- **Sources** (`src/source.rs`): `Sequential` has no length, skips by read-and-discard bounded by
  `Limits::skipped_block_bytes`, and returns `Unsupported` for a block behind the cursor. `Seekable`
  measures the stream length at construction, which enables the geometry-versus-length check in the
  pixel phase, and skips by seeking. FITS never needs `Seek`. XISF attachment positions are offsets
  from the start of the unit (§10.3). Both sources begin where the `Reader` is built: every
  declared position and the measured length count from the caller's cursor, so a frame that is
  one member of a larger stream reads in its own coordinates (`docs/implementation-decisions.md`).

Feature gates, all on by default: `fits` (hand-rolled, no dependency); `xisf` (`quick-xml`,
`lz4_flex` with `safe-decode` + `checked-decode`, `flate2` on `miniz_oxide`, `ruzstd`, `base64`);
`checksum` (`sha1`, `sha2`, `sha3`; inert without `xisf`). With the empty set the crate compiles and
every constructor returns `Unsupported`.

## What the crate promises on hostile input

`docs/design/2026-08-18-astroframe-library.md` is the specification the crate is graded against. A
finding is strongest when it falsifies one of these:

- **I5: no panic, no hang, no unbounded allocation for any input** (§ Invariants; § Errors,
  "Malformed input never panics … a contract, not an aspiration"; `src/lib.rs`). A panic found by
  the fuzzer is a release blocker (§ Hardening criteria, item 35). The contract covers *input*.
  Caller misuse returns `InvalidRequest` (item 31). The documented `# Panics` on a caller slice of
  the wrong length (`Normalizer::normalize_into`, `Chunk::normalize_into`) are programmer errors.
  The `unreachable!` / `expect` sites in `src/` guard internal invariants, so reaching any of them
  from input is a finding.
- **I4: no allocation from an unvalidated declared size** (§ Hardening: "Geometry is the ceiling;
  every other size the header declares is only ever a cross-check"). Buffers are sized from
  validated geometry bounded by the caps. A declared uncompressed size that disagrees with the
  geometry is `Malformed` before any allocation. Every codec inflates to exactly the
  geometry-implied length and then requires end of stream. The zstd window is checked before it is
  allocated. The XML header is read in 64 KiB chunks under its cap, never pre-sized.
- **Allocation proportional to input.** The fuzz oracle (`fuzz/src/lib.rs`, § Fuzzing) bounds
  *cumulative* allocation at `32 × input_len + 8 MiB` under tightened `Limits` (2¹⁸ samples, 1 MiB
  output). The rule behind it, from `docs/intentional-patterns.md`: **no per-occurrence allocation
  may be sized by a cap**. Anything allocated per image, per `<Reference>`, per subblock or per
  extension must be sized by that occurrence's own bytes, not multiplied by a cap the input chooses
  freely. This class has been found and fixed many times (gigabytes from headers of about a
  megabyte), and it is the most productive place to look.
- **No silent repair** (§ The organizing principle). A frame that cannot be decoded under the
  documented rules is an error, never a best guess. Returning `Ok` with pixels that did not come
  from the bytes the header declares for that image violates this.
- **Error classes and their order are specified** (§ Errors, § Errors → Validation order) and
  asserted by tests. **Decoded bits are public API** (§ Normalization).

## The caps

`Limits` (`src/limits.rs`) is a per-`Reader`, `Copy`, `#[non_exhaustive]` struct whose fields are
all public and settable by the caller. Tripping a cap is `LimitExceeded`. § The caps is the single
normative home of the caps.

| Field | Default | What it bounds |
| --- | --- | --- |
| `xml_header_bytes` | 8 MiB | Declared XISF header length, refused before the read. Also the real bound on `embedded` pixels (about 6 MB decoded) |
| `xml_depth`, `xml_elements` | 64, 100 000 | Parser nesting depth; elements (nodes) allocated |
| `attributes_per_element`, `attribute_value_bytes` | 4096, 1 MiB | Attributes parsed per element; one attribute's text |
| `subblock_count` | 4096 | Entries in one `subblocks` attribute. Their sums are also checked against the stored and geometry-implied sizes |
| `keyword_value_bytes` | 1 MiB | One assembled `CONTINUE` value, checked as it grows (FITS cards and XISF `FITSKeyword`s) |
| `stored_block_multiple`, `stored_block_floor_bytes` | 2×, 1 MiB | A declared `attachment:pos:size`, the one declared size that becomes an allocation, for every block held whole rather than streamed |
| `zstd_window_bytes` | 8 MiB | The window a zstd frame header declares |
| `fits_header_bytes`, `fits_header_cards` | 8 MiB, 4096 | 2880-byte blocks read while looking for `END`; cards stored |
| `hdus_per_advance` | 4096 | HDUs stepped over inside one `next_image()` |
| `total_samples` | 2³⁰ | The **file's** `w × h × c`, checked first in the pixel phase, before any buffer, regardless of `select_channel` |
| `decoded_output_bytes` | 2³⁰ | Bytes written to a tier-2 destination. Narrowed by `select_channel`; tier 3 is exempt |
| `materialized_bytes` | 2³⁰ | Each buffer the crate allocates for itself, at that buffer's own size: whole-image scratch, stored block, staging row, LZ4 subblock |
| `skipped_block_bytes` | 2³⁰ | One read-and-discard on a sequential source. Not consulted where skipping is a seek |
| `images_per_source` | 256 | `next_image()` advances. Occurrences past it are not built |

Size arithmetic is checked `u64`, narrowed once at the allocation site through `limits::narrow` (a
value that does not fit `usize` is `LimitExceeded`). The caps are not cumulative:
`materialized_bytes` applies per buffer, so a `Rows`-granularity decode of an image larger than the
cap is allowed. Caps also multiply, and the design records the joint header-only ceiling as about
1.05 GB from about 2.2 MB of input (§ The caps, "What the caps bound jointly"; § Fuzzing).

## XISF data-block locations: the crate opens nothing the input names

`parse_location` (`src/xisf/block.rs`) recognizes `attachment:<pos>:<size>` (and `attached:`),
`embedded`, `inline:<encoding>` and `url(…)` / `path(…)`. Only the first two are ever read:

- `attachment` is read from the same source at the declared offset. It is declined `Malformed` in
  the header phase if it lies inside the header region (`refuse_attachments_inside_the_header`,
  `src/xisf/decoder.rs`), and checked against the source length and the stored-block cap in the
  pixel phase.
- `embedded` is Base64 or lowercase hex text in the child `<Data>`, decoded during the header parse.
  Its `compression` and `subblocks` are read from `<Data>` (§10.6).
- `inline:` on an `<Image>` is declined `Malformed` (§11.5).
- `url(…)` / `path(…)` on an `<Image>` is declined `Malformed`, since §10.2 forbids external blocks
  in a monolithic unit, and `materialize` returns `Malformed` if one ever reaches it. On a
  `<Property>`, any `location` yields `PropertyValue::Unavailable` and nothing is read. Thumbnail,
  `ICCProfile` and `Table` blocks are never read.

The only filesystem access in `src/` is `std::fs::File::open` on the caller's own path in
`Reader::open`, and there is no network code. Distributed XISF is out of scope precisely because
`url` and `path` would mean fetches and filesystem traversal driven by the attacker (design doc,
§ Deferred and out of scope, item 2). **Any path by which the crate opens, stats or fetches a file,
path or URL named inside an input is Critical.**

## Checksums (`checksum` feature)

The API has no "verified" flag: a successful read is the verification. A block declaring
`checksum="alg:digest"` is hashed over its *stored* (compressed) bytes, and the digest must match
before anything decompresses, unshuffles or delivers it (§ Decisions the implementer must not
silently change, "Checksum before decompression, always"; XISF §10.6.1). A mismatch is
`ChecksumMismatch`: at construction for an `embedded` block, at the first pixel call for an
attachment. A checksummed block is never streamed (`is_streamable`), so no chunk precedes
verification. With the feature **off**, `codec::verify` returns `Unsupported` and the position
declines, so nothing unverified is delivered; `tests/checksum_feature_off.rs` covers this and runs
only under `--no-default-features --features fits,xisf`. The checksum is read from the element that
serializes the block, the `<Image>` (§10.5); for `embedded` only `compression` and `subblocks` move
to `<Data>`. Blocks the crate never reads (thumbnails, property blocks) are not verified, which
§10.5's on-demand rule permits. FITS `CHECKSUM` / `DATASUM` are reported and deliberately not
verified.

A file's author controls its digests, but verification still matters. A signed XISF unit seals its
attachments through checksums carried in the signed header (§9.5, §10.5), and a consumer that
verifies the XML signature by other means relies on this crate to enforce the block digests.
Delivering a sample from a block whose declared digest was not computed, or did not match, breaks
that seal.

## Components that matter most / least

Most important, in this order:

1. `src/xisf/`. `xml.rs` is a guarded pull parse into an arena (DOCTYPE refused; only character
   references and the five predefined entities resolve). `image.rs` and `keywords.rs` hold the
   per-occurrence metadata, `<Reference>` resolution and `CONTINUE` assembly across references,
   which is where the cap-multiplication family lives. `block.rs` and `codec.rs` cover location,
   compression and subblock parsing, decompression, unshuffle, verification and the streaming
   `BlockReader`. `decoder.rs` holds the pixel phase, the stored-block cap and the offset
   arithmetic.
2. `src/fits/`: `cards.rs` (card grammar, character set, `CONTINUE` / `HIERARCH` folding), `hdu.rs`
   (sizing keywords, the data-unit size formula, `INHERIT`), `decoder.rs` (HDU walk, skip, rows).
3. `src/reader.rs` (phase machine, cap ordering, chunk ranges), `src/source.rs` (skip, seek and
   position bookkeeping), `src/samples.rs`, `src/header.rs`, `src/metadata.rs`.
4. `src/normalize.rs`, a pure function of numbers the caller can see: bugs there are correctness,
   not memory safety.

Worth probing:

- Arithmetic that adds a declared offset or length to a product of the geometry: XISF `position`
  plus a row offset, subblock cursors, FITS `data_start`.
- Every loop whose length is bounded by a cap (`images_per_source`, `subblock_count`,
  `xml_elements`, `hdus_per_advance`, N references to one node).
- Subblocked blocks, which two code paths walk over the same list: streaming and materializing.
- Chunk `range()` against `destination_len()`. A caller indexing `buffer[chunk.range()]` in the
  documented idiom must never be made to panic.
- How declared positions are resolved for each source shape: sequential, seekable, and either one
  whose cursor is not at offset 0 when the `Reader` is built (`tests/pipeline.rs` pins the
  agreement for both formats).
- `select_channel` against the caps and staging buffers; re-reading an image on a seekable source;
  the rule that an error poisons the rest of that image (`pixels_failed`).

Lower priority: the empty and partial feature sets, and 32-bit `usize` narrowing (CI runs `i686`;
the target may not be installed here, so reason about it from `limits::narrow`).

Out of scope: `tools/`, `reference/` (a gitignored specification copy), `.github/`, `examples/`
(drivers), the harness code in `tests/` and `fuzz/`, and dev-dependencies such as `fitsrs`. Bugs
inside `quick-xml`, `lz4_flex`, `flate2` / `miniz_oxide`, `ruzstd`, `base64` or the RustCrypto
hashes are in scope only when astroframe hands the dependency input it was supposed to bound first
(a window, an output length, a header) or misuses its API.

## How to exercise it

The checkout is at `/src`. The image is offline (`CARGO_NET_OFFLINE=true`) with every dependency of
the root crate and of `fuzz/` (its own workspace) fetched. Already built: `cargo test --locked
--all-features --all-targets --no-run` and `cargo test --locked --no-default-features --features
fits,xisf --no-run`. `cargo +nightly fuzz build` (AddressSanitizer) is attempted too; nightly
drift can make it fail, and that failure is not a finding.

- **Tests.** `cargo test --locked --all-features` runs the suite, and `cargo test --locked
  --no-default-features --features fits,xisf` runs the checksum-off configuration. For one file:
  `cargo test --locked --all-features --test <name> -- <filter>` (`ls tests/`).
  `tests/COVERAGE.md` maps each invariant to its tests.
- **Overflow.** The test profile has `opt-level = 2` with overflow checks on, so an arithmetic
  overflow panics under `cargo test`. Rerun with `--release` to describe release behaviour
  (wraparound).
- **Fuzzing.** `cargo +nightly fuzz list`, then `cargo +nightly fuzz run <target> --
  -max_total_time=600`. Every target uses tightened `Limits` and panics when cumulative allocation
  exceeds the oracle bound, so an oracle trip shows up as a crash. The seeds in `fuzz/corpus/` are
  synthetic, and `tests/fuzz_replay.rs` replays them on stable.
- **No real-file corpus is available.** `ASTROFRAME_CORPUS` is unset and must stay unset, and the
  `#[ignore]`d corpus tests skip. Build inputs byte by byte, as the tests do, with the helpers in
  `tests/common/` (add `mod common;`). FITS: `Hdu`, `file`, `card`, `raw_card`. XISF: `xisf::Unit`,
  `attached_image`, `embedded_u16`, `raw_unit`, `zlib`, `lz4`, `zstd_raw`, `shuffle`,
  `checksum_attr`, `base64`, `hex`. Sources: `CountingRead`, `FailingRead`. Allocation:
  `alloc::{reset, total, peak}`, a counting global allocator. Its counter covers the whole process,
  so take each measurement inside one `#[test]` or run with `--test-threads=1`.
- **Sandbox.** 2 CPUs and 8 GB RAM. The default caps admit 1 GiB destinations, so lower the
  `Limits` in a reproducer when the claim is about proportion rather than absolute size.

## How you rate severity

A finding counts when a program that uses only the public API, with `Limits::default()` or a
tightened `Limits`, can be made to misbehave by bytes the attacker controls. Name the feature set.

- **Critical**: memory unsafety reachable from input (impossible in-crate under
  `forbid(unsafe_code)`, so it would mean a dependency fed out-of-contract input); the crate
  opening, stat-ing or fetching a file, path or URL named by the input.
- **High**: pixels or metadata delivered from bytes outside the part of the source the caller
  handed over. A documented cap bypassed so a small input drives memory or CPU far past what the cap
  names, or an abort (OOM, stack overflow), including allocation far above the fuzz oracle along a
  multiplier the input chooses. Non-termination, or a hang on a sequential source. Any sample
  delivered from a block whose declared checksum was not computed or did not match; decompression
  before verification; a checksummed block delivered with `checksum` off.
- **Medium**: a panic reachable from a public entry point on crafted input (it can be caught, but
  the contract says there are none, and the maintainer treats any panic as a release blocker). `Ok`
  returned with pixels that do not come from the bytes declared for that image: header bytes,
  zero-fill, a truncated block padded out, a stale buffer. Resource use above a cap or the oracle by
  a bounded factor. A decline bypassed so a declined feature decodes.
- **Low**: wrong decoded samples on valid input, a wrong error class, a validation-order
  divergence, or a cap tripping on a valid input it should admit. Report these too: the decoded bits
  and the error classes are public API.

Under raised `Limits` (up to `u64::MAX`, which the repository's own tests use), resource use within
what the raised caps name is not a finding. A panic or arithmetic overflow under raised caps is
still rated as above, because the crate explicitly guards geometry products against it
(`check_total_samples`, `src/reader.rs`).

## How reports and patches should look

- **Reproducer.** A Rust integration test under `tests/` that builds the input byte by byte in the
  test source with the `tests/common` builders. Never a binary blob, never a real-world frame: real
  frames carry observatory coordinates, and CI greps fixtures for `SITELAT`, `LAT-OBS`, `OBSGEO-*`
  and `Observation:Location:*`. A fuzz artifact may accompany the test but does not replace it.
- **Pixel comparisons** use `f32::to_bits()` (`common::assert_same_bits`), never `==`.
- **State** the entry point (`open`, `seekable` or `sequential`; tier; `select_channel`), the
  feature set, the `Limits`, debug and release behaviour, expected versus observed, and the root
  cause with file and line.
- **The patch** is the smallest change that keeps the public API, with its regression test next to
  the existing tests for that area (`adversarial.rs`, `fits_caps.rs`, `xml_guards.rs`, `bombs.rs`,
  `header_alloc.rs`, `peak_memory.rs`). Say explicitly whether it changes any decoded sample or
  error class for an input that decoded before, because that is a breaking change here.
- **Allocation fixes** share memory, or size by the occurrence's own bytes, rather than raising a cap
  or `ALLOC_MULTIPLE`. Allocation tests make repeated elements distinct; otherwise they measure a
  memo instead of the code (`docs/intentional-patterns.md`).
- **One root cause, one report**, listing every entry point (`open`, `seekable`, `sequential`, tier
  2, tier 3) and every feature set it reaches.

## Anything to leave alone

- **Documented declines are not bugs**: tile-compressed FITS (`ZIMAGE = T`), random groups,
  `NAXIS = 1` or `> 3`, zero-length axes, `Complex32/64`, `CIELab`, external and `inline` image
  locations, unknown codecs and checksum algorithms, FITS float or non-unsigned-convention scaling
  for *normalized* output, XISF signature verification, and block-valued properties
  (`Unavailable`). Every row of the design doc's § Deliberate divergences from prior art is
  intended too, for example decoding `Gray` with two channels or accepting `attached:`.
- **Allocation up to the caps from a small declaration is by design.** On a sequential source, a
  header of about a hundred bytes can drive the allocating `read_image()` / `read_samples()` up to
  `decoded_output_bytes` before the short read is discovered (§ Hardening).
- **The joint header-only ceiling is accepted.** About 1.05 GB from about 2.2 MB, reached by 256
  images that each compose a distinct `CONTINUE` chain through `<Reference>`, is pinned in
  `tests/header_alloc.rs`, and a fuzz run that reports it is triaged against § Fuzzing as accepted.
  The same figure reached by assembling *one* shared value repeatedly is a defect.
- **Caps that look like gaps but are not**: `skipped_block_bytes` is not consulted on seekable
  sources, `decoded_output_bytes` does not govern tier 3, `materialized_bytes` applies per buffer,
  and `total_samples` counts the file's channels even under `select_channel`. A borrowed
  destination's contents are unspecified after an error.
- **`src/normalize.rs` is deliberate.** It computes `((x − lo) as f32) × k` with
  `k = 1 / ((hi − lo) as f32)`, with no `mul_add`, a hand-written NaN-preserving clamp and a `-0.0`
  fix-up. `docs/intentional-patterns.md` explains each choice; do not simplify it.
- **Deliberate non-issues**: the digest comparison is not constant-time (digests are not secrets);
  SHA-1's weakness is the specification's choice; FITS `CHECKSUM` / `DATASUM` are not verified;
  `BLANK`, `PEDESTAL`, XISF `offset`, `ROWORDER` and `orientation` are reported and never applied.
- **Performance is out of scope.** Throughput and peak-memory ratios above the design's targets are
  not findings unless growth is unbounded.
