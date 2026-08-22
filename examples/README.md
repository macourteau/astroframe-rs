# Examples

Seven programs, simplest first. Each takes a frame path on the command line and works on both
FITS and XISF — that is the point of most of them.

```sh
cargo run --release --example 01_header -- /path/to/frame.fits
```

Build `--release` for anything that touches pixels. The debug profile decodes megapixel frames
slowly enough to be misleading about this crate's speed.

| | What it shows |
| --- | --- |
| `01_header` | **Start here.** Tier 1: geometry, sample format, bounds, streaming granularity — reading no pixel byte. Walks every image position and reports declined ones instead of failing. |
| `02_read_image` | Tier 2: a whole image as normalized `f32`, reusing one buffer across frames. Handles the frame that has no representable range rather than dying on it. |
| `03_native_samples` | The file's own integers, no normalization. This is the path that reads *every* file, including the ones tier 2 refuses. |
| `04_metadata` | Keywords and properties, and the exact-match lookup that does not case-fold. |
| `05_streaming` | Tier 3: chunked delivery, and `Reader::sequential` over a pipe. Reads `granularity()` first to decide whether streaming buys anything. |
| `06_untrusted` | `Limits` and the error classes — the shape to use for input you did not produce. |
| `07_channels_and_bounds` | `select_channel`, and `with_bounds` as the escape hatch for a frame whose range the file does not state. |

## What they are trying to teach

- **Report, don't interpret.** The crate hands back what the file said, in the file's spelling.
  A `DATE-OBS` is the text on the card. Policy — autostretching, blank masking, choosing a
  range for a frame that declares none — lives in your code, which is why `07` implements the
  autostretch itself rather than asking the crate for one.
- **A decline is not an error.** A position this version will not read reports a class and a
  sentence, and the rest of the file still walks. Treating it as a failure throws away the
  images that do decode. Every example checks `decline_reason()` before the geometry.
- **The `Option`s are real.** Past `decline_reason()` the geometry accessors are present; before
  it they may not be. None of them is an `unwrap` waiting to happen.
- **Ask before you decode.** `granularity()` says how much of the input must be held before any
  sample comes out, and `bounds()` says whether normalized output exists at all — both before a
  pixel is read.
- **`Limits` is the knob for hostile input.** The defaults are sized for frames you produced.
  `06` shows which ones matter and why.

## CI, and what is not in it

CI compiles them with `-D warnings` on every push. That is a compile-time
contract on the public surface: nothing else in the pipeline fails when an API an example
demonstrates gets renamed, because `cargo test` does not build examples and a doc comment
cannot call a method.

CI does not *run* them, because running them needs a frame, and real frames are not in the
repository. Running them against real files is a local step:

```sh
for e in examples/*.rs; do
  n="$(basename "$e" .rs)"
  echo "=== $n ==="
  cargo run --release --quiet --example "$n" -- "$ASTROFRAME_CORPUS/some/frame.fits"
done
```
