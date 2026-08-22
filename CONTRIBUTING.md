# Contributing to astroframe

Thanks for looking at this. The crate is small and deliberately opinionated, so the most useful
thing you can read before changing behaviour is
[the design document](https://github.com/macourteau/astroframe-rs/blob/main/docs/design/2026-08-18-astroframe-library.md) — it is a decision record
rather than an interface listing, and it says why each rule is what it is.

## Read this first if you are about to simplify the arithmetic

[`docs/intentional-patterns.md`](https://github.com/macourteau/astroframe-rs/blob/main/docs/intentional-patterns.md) exists because parts of
`src/normalize.rs` look wrong and are not. The normalization form is
multiply-by-rounded-`f32`-reciprocal rather than the idiomatic division, and the two disagree
on 0.78% of 16-bit levels by one ULP. **The decoded bits are part of the public API**, so that
difference is a breaking change, not a cleanup.

## Run this before opening a pull request

Every one of these runs locally, and CI runs the same things. CI is confirmation, not
discovery.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

For anything touching features, the numeric path, or packaging, also run:

```sh
# The feature powerset. The empty set is a supported configuration: with both formats off
# the crate still compiles and exposes the header types and the normalization primitive,
# and every constructor returns Unsupported.
for f in "" fits xisf checksum fits,xisf fits,checksum xisf,checksum fits,xisf,checksum; do
  cargo build --locked --no-default-features --features "$f" || echo "FAILED: [$f]"
done

# The MSRV is reported, not targeted — the check exists so the declared number stays true.
#
# Invoke the toolchain's own cargo AND rustc by path. `rustup run 1.88 cargo …` is NOT
# enough: if another Rust is earlier on PATH (a Homebrew install, say), rustup's shim loses
# and the check silently runs on the wrong compiler. That is a *false pass*.
rustup toolchain install 1.88 --profile minimal
RT=~/.rustup/toolchains/1.88-$(rustc -vV | sed -n 's/^host: //p')/bin
RUSTC=$RT/rustc $RT/cargo check --locked --all-features --all-targets

cargo deny --all-features check licenses
cargo package --locked --all-features
```

## Tests and fixtures

Fixtures are built **byte by byte in the test source**, never checked in as opaque blobs. Two
reasons, and the second is the one that matters: a fixture nobody can read cannot be reasoned
about when an exhaustive bit-comparison fails, and a committed frame is how real observatory
coordinates and timestamps get published by accident. `test-data/` stays gitignored, and so
does anything derived from a real frame — a fuzz seed, a fixture, an issue report, a pasted
header.

Compare pixel buffers with `f32::to_bits()`, never `==`. `==` silently accepts sign-of-zero
differences, which is exactly the class of defect the endpoint tests exist to catch.

Some tests are `#[ignore]`d and read a corpus of real frames through `ASTROFRAME_CORPUS`.
That corpus is not distributable — the frames carry observatory coordinates at full precision
— so those tests are maintainer-only and no CI lane may set the variable. Everything CI
grades is fixture-borne.

## Licensing

The crate is written from FITS Standard 4.0 and the XISF 1.0 specification alone, and verified
against real files. That is what keeps it permissively licensable, so please do not introduce
material derived from any other implementation. The XISF specification embeds a C++ reference
implementation of the byte-shuffling transform; a specification that ships sample code is
still someone's copyrighted code, and every algorithm here is written from the *described*
transform.

Contributions are taken under `MIT OR Apache-2.0`, matching the crate.

## Prose conventions

Documentation is written as if the code had always been this way — no "new" or "now" language.
Where history matters it belongs in a changelog section, not in the sentence describing the
behaviour. This is the convention most easily broken by accident.
