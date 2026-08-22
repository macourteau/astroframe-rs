# astroframe — repository conventions

## The design document is the source of truth

[`docs/design/2026-08-18-astroframe-library.md`](https://github.com/macourteau/astroframe/blob/main/docs/design/2026-08-18-astroframe-library.md)
(`Status: Implemented`) is the specification this crate is graded against, and it is a decision
record rather than an interface listing. Before changing behaviour, read the section that owns
the rule. The five load-bearing homes are § Normalization (the arithmetic and the range),
§ The organizing principle (report, don't interpret), § Errors → Validation order (order and
classes), § The caps (every limit), and § Deliberate divergences from prior art.

Choices the document leaves open, and the resolutions taken, are in
[`docs/implementation-decisions.md`](https://github.com/macourteau/astroframe/blob/main/docs/implementation-decisions.md). Code that looks wrong
and is not is in [`docs/intentional-patterns.md`](https://github.com/macourteau/astroframe/blob/main/docs/intentional-patterns.md) — read that
before "simplifying" anything in `src/normalize.rs`.

## Releases are driven by `Cargo.toml`, not by commit messages

**This repo does not use conventional-commit version computation.** The release workflow reads
`version` from `Cargo.toml` and creates `v$version` if that tag does not exist. Bumping the
version is the release action.

That inversion is deliberate, and the second reason is the decisive one:

1. A crate published to crates.io must carry the correct version in its manifest before it is
   published at all, so the manifest is authoritative whether or not CI agrees. Computing the
   version from commit messages instead needs a bump commit that races the pipeline.
2. **No commit-message convention can compute this crate's version.** The decoded bits are
   part of the public API: a release that changes one ULP of output for an input it previously
   decoded is a **breaking** change, which at `0.x` means `0.1 → 0.2` and never
   `0.1.0 → 0.1.1`. Whether a change moves a bit is a judgement about arithmetic, not a
   property of a `fix:` prefix.

So before bumping: if the change can alter a decoded sample for any input that decoded before,
it is a minor bump at `0.x` and a major after `1.0`. The exhaustive tests in
`tests/normalization.rs` are what tell you.

Conventional-commit prefixes (`feat:`, `fix:`, `chore:`, `docs:`) are still house style for
readability. They carry no release semantics here.

## Local verification — run this before pushing

Every command below runs locally. None of them should be discovered by a red pipeline: CI is
confirmation, not discovery.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features

# The feature powerset. The empty set is a supported configuration: with both formats off
# the crate still compiles and exposes the header types and the normalization primitive,
# and every constructor returns Unsupported.
for f in "" fits xisf checksum fits,xisf fits,checksum xisf,checksum fits,xisf,checksum; do
  cargo build --locked --no-default-features --features "$f" || echo "FAILED: [$f]"
done

# The MSRV is reported, not targeted — the job exists so the declared number stays true.
#
# Invoke the toolchain's own cargo AND rustc by path. `rustup run 1.88 cargo …` is NOT
# enough: if another Rust is earlier on PATH (a Homebrew install, say), rustup's shim loses
# and the check silently runs on the wrong compiler. That is a *false pass* — it let a
# let-chain through a 1.87 check locally and CI caught it instead.
rustup toolchain install 1.88 --profile minimal
RT=~/.rustup/toolchains/1.88-$(rustc -vV | sed -n 's/^host: //p')/bin
RUSTC=$RT/rustc $RT/cargo check --locked --all-features --all-targets

cargo deny --all-features check licenses
cargo package --locked --all-features
```

The grep-shaped checks in `.github/workflows/ci.yml`'s `greps` job are plain shell — read them
there and run them directly if you touched fixtures, the normalization module, or `reference/`.

## The corpus is never a CI dependency

The 84 GB local corpus lives outside the repository, is reached only through
`ASTROFRAME_CORPUS`, and backs `#[ignore]`d tests:

```sh
ASTROFRAME_CORPUS=/path/to/corpus cargo test --release -- --ignored
```

No CI lane may set that variable — the `build-test` job fails if it is set. The corpus carries
observatory coordinates at full precision, and not all of the frames are the maintainer's own.

## Fixtures

Built **byte by byte in the test source**, never checked in as opaque blobs. A fixture nobody
can see cannot be reasoned about when an exhaustive bit-comparison fails, and a committed
frame is how real observatory coordinates and timestamps get published by accident. `test-data/`
stays gitignored.

Compare pixel buffers with `f32::to_bits()`, never `==` — `==` silently accepts sign-of-zero
differences, which is exactly the class of defect the endpoint tests exist to catch.

## Licensing discipline

The crate is written from FITS Standard 4.0 and the XISF 1.0 specification alone, and verified
against real files. That is what keeps it permissively licensable, so do not introduce material
derived from any other implementation. The converted XISF specification under `reference/` is
**not** redistributed and stays gitignored.

## Prose conventions

No "new"/"now" language: write as if the code has always been this way. Where history matters,
it belongs in a changelog section rather than in the sentence describing the behaviour. Every
document in this repository follows this, and it is the convention most easily broken by
accident.
