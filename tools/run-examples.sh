#!/usr/bin/env bash
#
# Run every example against real frames and check that each one exits cleanly.
#
# CI compiles the examples with `-D warnings`, which is a contract on the public surface and
# nothing else: an example can compile and still be wrong the moment it meets a file. Running
# them needs frames, frames are never committed here, so this is the maintainer-local half —
# the same arrangement, and the same variable, as the `#[ignore]`d tests in `tests/corpus.rs`.
#
#     ASTROFRAME_CORPUS=/path/to/corpus tools/run-examples.sh
#
# Unset variable: skip and exit 0, so this is safe to leave in a local pre-push sweep. **No CI
# lane may set it** — the corpus carries observatory coordinates at full precision, and no path
# to it belongs in a committed file.
#
# What counts as clean
# --------------------
#
# The exit status, never the printed text. Pinning the output would need fixtures encoding real
# frame metadata, which is exactly what must not enter this tree.
#
#   0            decoded, or reported what it found. A pass.
#   1            refused, with the reason on stderr. Counted and printed, not fatal: a corpus
#                of real frames contains files that are *supposed* to be refused — a block
#                whose digest disagrees with its header, a frame over the caps `06_untrusted`
#                deliberately sets tight — and an example that dies on those is behaving.
#   anything     a panic (101), an abort, or the runner calling the example wrong (2). Fatal.
#                The no-panic guarantee covers malformed input, so 101 here is a real defect.
#
# The one thing status alone cannot catch is an example broken against every frame, which would
# look like an unbroken row of orderly refusals. So each example must also come back 0 at least
# once over the pool; never succeeding is a failure however tidy the refusals were.
#
# Knobs, both with usable defaults:
#
#   ASTROFRAME_EXAMPLE_FRAMES      how many frames to sweep (default 32, about a minute)
#   ASTROFRAME_EXAMPLE_MAX_MIB     largest frame to consider, in MiB (default 256)

set -euo pipefail

if [ -z "${ASTROFRAME_CORPUS:-}" ]; then
    echo "ASTROFRAME_CORPUS is unset; there are no frames to run against. Skipping."
    exit 0
fi
if [ ! -d "$ASTROFRAME_CORPUS" ]; then
    echo "ASTROFRAME_CORPUS is set but is not a directory: $ASTROFRAME_CORPUS" >&2
    exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
wanted="${ASTROFRAME_EXAMPLE_FRAMES:-32}"
max_mib="${ASTROFRAME_EXAMPLE_MAX_MIB:-256}"
# Honouring CARGO_TARGET_DIR matters: a worktree sharing a target directory would otherwise
# build into one place and look for the binaries in another.
examples="${CARGO_TARGET_DIR:-$root/target}/release/examples"

# `--release`, because the examples say so themselves: the debug profile decodes megapixel
# frames slowly enough to turn a sweep into an afternoon.
cargo build --release --locked --all-features --examples --manifest-path "$root/Cargo.toml"

list="$(mktemp)"
trap 'rm -f "$list"' EXIT
# A directory named `negative/` holds files broken on purpose, and their whole value is that
# the decoder rejects them — so every example would refuse every one of them, every run, and
# the report would say nothing. Pruned by name, as the corpus-backed tests prune it.
find "$ASTROFRAME_CORPUS" -type d -name negative -prune -o -type f \
    \( -iname '*.fits' -o -iname '*.fit' -o -iname '*.fts' \
    -o -iname '*.xisf' -o -iname '*.xisb' \) \
    -size "-$((max_mib * 1024))k" -print | sort >"$list"

total="$(wc -l <"$list" | tr -d ' ')"
if [ "$total" -eq 0 ]; then
    echo "no frames under ${max_mib} MiB in $ASTROFRAME_CORPUS" >&2
    exit 1
fi

# An even spread through the sorted list rather than the first N. A corpus is organized by
# family — one directory of FITS variants, one of XISF codec permutations, one of integrated
# masters — so the head of it is a hundred near-identical files and proves almost nothing.
step="$(((total + wanted - 1) / wanted))"
[ "$step" -ge 1 ] || step=1
pool="$(awk -v step="$step" 'step == 1 || NR % step == 1' "$list")"
frames="$(printf '%s\n' "$pool" | wc -l | tr -d ' ')"

echo "$frames of $total frames under ${max_mib} MiB, from $ASTROFRAME_CORPUS"
echo

failures=0
passed=0
refused=0
broke=0

# Grade one run and say what happened, loudly for the two outcomes that need reading.
record() {
    local status="$1" label="$2" frame="$3" output="$4"
    case "$status" in
    0) passed=$((passed + 1)) ;;
    1)
        refused=$((refused + 1))
        printf 'refused: %s — %s\n' "$label" "${frame#"$ASTROFRAME_CORPUS"/}"
        printf '%s\n' "$output" | sed 's/^/    /'
        ;;
    *)
        broke=$((broke + 1))
        printf 'FAIL (exit %s): %s — %s\n' "$status" "$label" "${frame#"$ASTROFRAME_CORPUS"/}" >&2
        printf '%s\n' "$output" | sed 's/^/    /' >&2
        ;;
    esac
}

# Close out one example's row and fold it into the totals.
tally() {
    local label="$1"
    if [ "$passed" -eq 0 ]; then
        printf 'FAIL: %s never succeeded on any of the %s frames\n' "$label" "$frames" >&2
        broke=$((broke + 1))
    fi
    failures=$((failures + broke))
    printf '%-38s %3d ok  %3d refused  %3d failed\n' "$label" "$passed" "$refused" "$broke"
    passed=0
    refused=0
    broke=0
}

# One example over the whole pool. Every one of them takes the frame first, so anything passed
# here follows it.
sweep() {
    local label="$1" example="$2"
    shift 2
    while IFS= read -r frame; do
        [ -n "$frame" ] || continue
        local status=0 output
        # stdin is closed rather than inherited: `05_streaming` reads a frame from it when
        # given no path, and one stray invocation would sit waiting on the terminal.
        output="$("$examples/$example" "$frame" "$@" 2>&1 </dev/null)" || status=$?
        record "$status" "$label" "$frame" "$output"
    done <<<"$pool"
    tally "$label"
}

# The forward-only path, which is the one thing about `05_streaming` a path argument cannot
# reach: the frame arrives on stdin and the source refuses to seek.
sweep_stdin() {
    local label="$1" example="$2"
    while IFS= read -r frame; do
        [ -n "$frame" ] || continue
        local status=0 output
        output="$("$examples/$example" <"$frame" 2>&1)" || status=$?
        record "$status" "$label" "$frame" "$output"
    done <<<"$pool"
    tally "$label"
}

sweep "01_header" 01_header
sweep "02_read_image" 02_read_image
sweep "03_native_samples" 03_native_samples
# Both branches: the dump, and the named lookup that does not case-fold. A frame missing one of
# these names reports that it is missing rather than failing, which is the branch worth having.
sweep "04_metadata" 04_metadata
sweep "04_metadata DATE-OBS EXPTIME" 04_metadata DATE-OBS EXPTIME NAXIS1
sweep "05_streaming" 05_streaming
sweep_stdin "05_streaming <stdin" 05_streaming
sweep "06_untrusted" 06_untrusted
sweep "07_channels_and_bounds" 07_channels_and_bounds
# Channel 0 exists in every frame, so `select_channel` is exercised over the whole pool rather
# than only over the colour frames in it.
sweep "07_channels_and_bounds 0" 07_channels_and_bounds 0

echo
if [ "$failures" -eq 0 ]; then
    echo "all examples ran clean over $frames frames"
else
    echo "$failures failed" >&2
fi
[ "$failures" -eq 0 ]
