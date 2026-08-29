# Changelog

Every published version is recorded here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## How this crate versions

**The decoded bits are part of the public API.** A release that changes one ULP of output for
an input a previous release decoded is a breaking change, which below `1.0` means `0.1 → 0.2`
and never `0.1.0 → 0.1.1`. Whether a change moves a bit is a judgement about arithmetic rather
than a property of a commit-message prefix, so the exhaustive tests in `tests/normalization.rs`
and a cross-revision comparison over the corpus are what settle it. **Every entry below states
whether decoded output moved, and what was compared to establish it.**

A *decline* is not a decode: a position the crate refuses with a stated reason has no decoded
output to move. Entries that change what is declined say so, because that is visible to a
caller even though no sample changes.

## [0.2.1] — 2026-08-28

Decoded output does not move. No change in this release touches the normalization primitive
or any sample path.

### Fixed

- A `HIERARCH` card whose keyword-name field held nothing but non-ASCII whitespace was accepted
  as an ordinary keyword named `HIERARCH`, carrying bytes outside `0x20`–`0x7E` into the comment
  field, which tolerates them — while the identical bytes beside a real word were a hard error.
  The cause was a Unicode predicate deciding a byte-oriented format's rule: `split_whitespace`
  treats `U+00A0` as whitespace, so the field was discarded as blank before the character set
  was checked. The character set is now checked first. **This changes what is accepted**: such a
  card is `Error::Malformed`, which is what § Header character set specifies for a keyword-name
  field. No sample decodes differently.

### Changed

- `Bounds::CallerSupplied` and `Granularity::Block` no longer carry `#[non_exhaustive]` on the
  variant. Both are reported by the decoder and never accepted as input, so no production caller
  constructs one, and the annotation charged its whole cost to downstream test doubles, mocks and
  fuzz harnesses. The enums keep theirs, so adding a *variant* remains additive. Removing it only
  relaxes a restriction, so this breaks nothing.
- `Reader::header` is deprecated in favour of `Reader::current_header`. Its `None` reports a
  caller error — asking before the first `next_image` — which `current_header` states as
  `Error::InvalidRequest`. Both return the same value while both exist, and a test asserts it.

### Documentation

- This file. The published crate carried no history at all: `/docs` and `CLAUDE.md` are excluded
  from the tarball, so a consumer without the repository checked out had nothing to migrate
  against.
- Three rustdoc references to files under `docs/` are absolute links rather than relative paths.
  `/docs` is excluded from the published tarball, so the relative form pointed at nothing for
  anyone reading the crate on crates.io or docs.rs.

## [0.2.0] — 2026-08-28

**Decoded output does not move.** Verified twice, by independent means: a cross-revision digest
comparison over 879 corpus positions reported 0 bits moved and 0 classifications changed, and a
re-run of the public-archive validation over 163 archive frames found all 410 decoded positions
byte-identical to 0.1.1 by SHA-256, and byte-identical to independent reference decoders.
A downstream consumer reports a third, independent confirmation on the FITS side: its golden
checkpoint replays, graded against a locally-compiled C oracle, were byte-identical across the
same bump. That evidence is theirs rather than reproducible from this repository, and it
corroborates the two above rather than standing in for either.

The surface a caller meets, reshaped once while that is still cheap.

### Breaking

- `Bounds` variants carry the validated `SampleRange` their producers already build, instead of
  unlabelled `f64` pairs every tier-3 caller had to revalidate against a branch no producer can
  reach.
- `Range` is `SampleRange`, because `Chunk::range` returns `std::ops::Range` a few lines away in
  the same loop and the crate had been aliasing internally to hold both.
- `Header::get` is `Header::keyword`, named for the surface it searches, like `Header::property`
  beside it.
- `Reader::with_bounds` is `Reader::set_bounds`: `with_` is the builder prefix `Limits` uses, and
  this is a fallible setter whose sibling is `select_channel`.
- `KeywordIter` and `PropertyIter` are newtypes rather than aliases for `std::iter::Chain`, which
  had pinned the number of internal pieces into the public contract and forfeited
  `ExactSizeIterator` over an O(1) length.
- Each public item has one path rather than two: the modules are private and the items are
  re-exported at the root.
- `Source`'s methods sit on the sealed trait, so the rendered page shows only what a caller can
  use.

### Added

- `Reader::normalizer` and `Chunk::normalize_into`, which make tier 3 *plus normalization*
  reachable. The design document promises a caller can normalize a chunk with the same primitive;
  until now the private ones meant the streaming-equals-whole-buffer test graded a verbatim copy
  of the shipped code rather than the code.
- `Reader::current_header`, which returns the header a caller has already established is there.
- `Format`, reporting which container a file is. Seven accessors had been documenting it sideways
  through their own `None` conditions.
- `Header::property`, `Header::geometry`, `PropertyValue::as_str`, `Reader::destination_len`,
  `Reader::read_samples`, `Reader::is_seekable`, `SampleSlice::iter_f64` with the named `F64Iter`
  cursor, `Error::decline_class`, `Error::is_invalid_request`, `impl From<&DeclineReason> for
  Error`, `as_str` and `Display` across the reported enums, `Image::into_parts` and
  `AsRef<[f32]>`, and `Limits::with_*`.
- Every one of the eighteen fallible public items carries an `# Errors` section.

### Fixed

- The XISF materializing path reused one codec state across a block's subblocks, so a
  multi-subblock block decoded wrongly after the first.
- A subblock's input buffer was sized by the whole block rather than by the subblock: a 139,513
  byte input allocated 311,935,184 bytes through zlib, and zstd had the same defect.
- Ancillary XISF elements report or decline; none is silently repaired.
- The XISF XML walk resolves namespace prefixes in constant rather than quadratic time — 284× on
  a 7.4 MB header — and refuses a truncated document rather than reading past it.
- A FITS card's value carries its kind, and the sizing keywords are read once.

### Changed

- The crate description states that it is pure Rust, which its dependency graph was verified to
  be.
- The release lane runs the test suite before it publishes.
- The README states which FITS and XISF forms are supported, and how each is tested.

## [0.1.1] — 2026-08-22

Decoded output does not move: no file under `src/` changed.

### Changed

- The repository is `astroframe-rs`, matching the `-rs` suffix its siblings carry. The published
  crate remains `astroframe`. Documentation links point at the new path, and the crates.io
  Trusted Publishing configuration — which matches on repository name — was recreated against it,
  a rename having otherwise broken publishing silently.

## [0.1.0] — 2026-08-22

Initial release: a decode-only reader for FITS 4.0 and XISF 1.0 astronomical image frames,
written from the two specifications alone and verified against real files.

Three tiers — header-only, whole-image decode, and chunked streaming — over one normalization
primitive shared by both containers, so the two formats cannot drift apart. Every limit is a
documented cap, a frame the crate cannot decode under its stated rules is an error rather than a
best guess, and a position this version does not handle is *declined* with a class and a reason
while the walk continues.

[0.2.1]: https://github.com/macourteau/astroframe-rs/releases/tag/v0.2.1
[0.2.0]: https://github.com/macourteau/astroframe-rs/releases/tag/v0.2.0
[0.1.1]: https://github.com/macourteau/astroframe-rs/releases/tag/v0.1.1
[0.1.0]: https://github.com/macourteau/astroframe-rs/releases/tag/v0.1.0
