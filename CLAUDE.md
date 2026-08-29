# astroframe — repository conventions

## The design document is the source of truth

[`docs/design/2026-08-18-astroframe-library.md`](https://github.com/macourteau/astroframe-rs/blob/main/docs/design/2026-08-18-astroframe-library.md)
(`Status: Implemented`) is the specification this crate is graded against, and it is a decision
record rather than an interface listing. Before changing behaviour, read the section that owns
the rule. The five load-bearing homes are § Normalization (the arithmetic and the range),
§ The organizing principle (report, don't interpret), § Errors → Validation order (order and
classes), § The caps (every limit), and § Deliberate divergences from prior art.

Choices the document leaves open, and the resolutions taken, are in
[`docs/implementation-decisions.md`](https://github.com/macourteau/astroframe-rs/blob/main/docs/implementation-decisions.md). Code that looks wrong
and is not is in [`docs/intentional-patterns.md`](https://github.com/macourteau/astroframe-rs/blob/main/docs/intentional-patterns.md) — read that
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

A bump also settles outstanding deprecations, and `tests/deprecations.rs` is what makes that
mechanical rather than remembered: each deprecated item has a test asserting the version has not
yet reached its removal line. Bumping past that line fails the suite with the item named. A
failure there is the mechanism working — delete the item, then delete its test.

Every entry in `CHANGELOG.md` states whether decoded output moved and what was compared to
establish it. A release that cannot answer that question is not ready to be tagged.

## Local verification — run this before pushing

Every command below runs locally. None of them should be discovered by a red pipeline: CI is
confirmation, not discovery.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features

# `missing_docs` and broken intra-doc links are both errors in CI, and neither clippy nor the
# doctests reach them — only rustdoc does. Specification citations are what break this: a
# reference written `[23]` reads to rustdoc as a link to an item named `23`.
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --all-features

# The feature powerset. The empty set is a supported configuration: with both formats off
# the crate still compiles and exposes the header types and the normalization primitive,
# and every constructor returns Unsupported.
for f in "" fits xisf checksum fits,xisf fits,checksum xisf,checksum fits,xisf,checksum; do
  cargo clippy --locked --all-targets --no-default-features --features "$f" \
    -- -D warnings || echo "FAILED: [$f]"
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

# `cargo fuzz` loses the same fight, and needs nightly for `-Zsanitizer`. Neither
# `rustup run nightly cargo fuzz` nor `RUSTUP_TOOLCHAIN=nightly cargo fuzz` survives a
# Homebrew Rust earlier on PATH: both run stable, and stable rejects the sanitizer flag, so
# this one costs an hour of reading the wrong error rather than passing falsely. Put the
# toolchain's own directory ahead of PATH instead.
NT=~/.rustup/toolchains/nightly-$(rustc -vV | sed -n 's/^host: //p')/bin
PATH=$NT:$PATH cargo fuzz run <target>

cargo deny --all-features check licenses
cargo package --locked --all-features

# The resident-memory half of § Streaming's peak-memory criterion. Maintainer-local rather
# than per-push: it reads `/proc`, so it is Linux-only, and it measures what the operating
# system actually holds, which needs a quiet machine to mean anything. Run it beside the
# corpus run below. The allocation half runs on every push and is covered by `cargo test`.
#
# `--nocapture` is load-bearing. Off Linux the test skips and still reports `ok`, so without
# the printed figures a skip and a measurement are indistinguishable — which is how this half
# of the criterion went unmeasured while appearing to pass.
cargo test --release --all-features --test peak_memory_resident -- --ignored --nocapture
```

Off Linux, run it in a container rather than reading that skip as a pass. `git archive` rather
than a bind mount, so the tag is what gets measured and no target directory rides along:

```sh
git archive --format=tar HEAD > /tmp/astroframe-src.tar
docker run --rm -v /tmp/astroframe-src.tar:/src.tar:ro rust:1 bash -c '
  mkdir -p /work && tar xf /src.tar -C /work && cd /work
  cargo test --release --locked --all-features \
    --test peak_memory_resident -- --ignored --nocapture'
```

The grep-shaped checks in `.github/workflows/ci.yml`'s `greps` job are plain shell — read them
there and run them directly if you touched fixtures, the normalization module, or `reference/`.

## The corpus is never a CI dependency

The 84 GB local corpus lives outside the repository, is reached only through
`ASTROFRAME_CORPUS`, and backs `#[ignore]`d tests:

```sh
ASTROFRAME_CORPUS=/path/to/corpus cargo test --release -- --ignored

# The examples, which CI compiles with `-D warnings` and never runs — running one needs a
# frame. This is the other half of that contract: it sweeps every example over a spread of
# corpus frames and grades the exit status. Unset variable, and it skips and exits 0.
ASTROFRAME_CORPUS=/path/to/corpus tools/run-examples.sh
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
