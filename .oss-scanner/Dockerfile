# The image Anthropic's OSS Scanner (https://github.com/anthropics/oss-scanner) audits this crate in.
#
# The scanner builds this file with the repository checkout as the build context and the network
# available, then studies the finished image with no network at all. So everything a reproducer
# needs is fetched and compiled here, and `threat_model.md` beside this file says how to use it.
#
# The toolchains float rather than pin. Nothing here gates a merge: `ci.yml` grades the MSRV and
# the pinned CI toolchain, and the fuzz lane in `heavy.yml` floats nightly and cargo-fuzz the same
# way. A pin would only add update traffic for an image the scanner rebuilds from scratch.
FROM rust:1-trixie

ENV CARGO_TERM_COLOR=never \
    RUST_BACKTRACE=1 \
    RUSTFLAGS="-C force-frame-pointers=yes"

# cargo-fuzz builds on stable; only the fuzz targets need nightly, for the sanitizer flags.
RUN rustup toolchain install nightly --profile minimal \
    && cargo install --locked cargo-fuzz

# Scanner contract: the checkout lives inside the image, at /src.
COPY . /src
WORKDIR /src

# `fuzz/` is its own workspace with its own lockfile, so the root fetch does not reach it.
RUN cargo fetch --locked \
    && cargo fetch --locked --manifest-path fuzz/Cargo.toml

# The two feature configurations whose test sets differ. `--all-features` builds every target,
# examples included; `tests/checksum_feature_off.rs` only runs with the checksum feature off. A
# step that fails here fails the image.
RUN cargo test --locked --all-features --all-targets --no-run \
    && cargo test --locked --no-default-features --features fits,xisf --no-run

# The fuzz targets, with AddressSanitizer. A nightly that cannot build them must not fail the
# image: their dependencies are fetched above either way, and `tests/fuzz_replay.rs` replays the
# committed corpus on stable.
RUN cargo +nightly fuzz build || echo "WARNING: the fuzz targets did not build"

# Run both configurations so the build log shows the image works, without letting a failure
# abort it.
RUN (timeout 1200 cargo test --locked --all-features \
        || echo "WARNING: cargo test --all-features reported failures") \
    && (timeout 1200 cargo test --locked --no-default-features --features fits,xisf \
        || echo "WARNING: cargo test --features fits,xisf reported failures")

# Everything is fetched. From here on cargo must not wait for a registry it cannot reach.
ENV CARGO_NET_OFFLINE=true
