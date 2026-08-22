# astroframe — FITS and XISF decode library

Status: Implemented
Design date: 2026-08-18

## Problem

Astronomical imaging sessions produce hundreds of frames per night in two container
formats: **FITS**, the long-standing astronomy standard, and **XISF**, PixInsight's
native format. Every tool that touches those frames has to open them first — a grader
scoring star shape and focus, an ingest service cataloguing a night's take, a
calibration step subtracting a master dark, a viewer rendering a preview. They want
very different things from the file: one needs every pixel to the last bit, one needs
only the header, one needs the raw sensor values rather than a normalized image.

That shared decode step is the subject of this document.

`astroframe` is a Rust library that reads a FITS or XISF frame and produces two
things: the header metadata a tool needs (geometry, exposure, gain, pixel scale,
pointing, timestamps) and the pixels themselves — either in the file's own sample
type, or normalized to `f32` in `[0,1]`, row-major and channel-planar. It is
decode-only: nothing in it writes a frame.

**It is a separate library so that several projects can share it.** That is the
reason, and it is the whole reason: decoding FITS and XISF is work every
astrophotography tool needs and none of them should each do differently. A module
inside one consumer would be a decoder that only that consumer can use, and the
divergence between two such decoders is precisely the class of defect this design
exists to prevent.

A second property follows from that choice rather than motivating it. The crate is
written from published specifications alone, which is what makes it publishable under a
permissive licence — a welcome consequence, not the premise. § Licensing boundary
records the discipline that keeps it true.

### How this document is scoped

One rule governs what is written here and what is not:

> **The specifications decide the formats. This document decides what they leave
> open. The acceptance criteria pin the behaviour by test.**

So a rule that appears in FITS Standard 4.0 or in the XISF 1.0 specification is
**cited by section number and not restated** — an implementer reads it there, where it
is normative, rather than here, where a paraphrase can drift. What this document spends
its length on is the residue: the conventions the specifications are silent about, the
divergences taken deliberately, the API, the memory and safety mechanisms, and the
tests that make all of it checkable.

Local copies: FITS Standard 4.0 is public
(<https://fits.gsfc.nasa.gov/fits_standard.html>); the XISF specification is converted
to `reference/xisf-1.0-spec.md` by `tools/xisf-spec-to-md.py` and is **not
redistributed** — see `reference/README.md` for how to regenerate it. Two converter
artifacts matter when reading that copy: equations are stripped to empty image
references (so §8.5.5's Equation [18] and the shuffle transform's [33]–[35] are legible
only from surrounding prose), and normative negations are fused — `shall not`, `must not`
and `should not` appear as `shallnot`, `mustnot` and `shouldnot`. Grepping the local copy
for the spaced forms silently misses almost every prohibition in the specification.

### Licensing boundary

`astroframe` is written from FITS Standard 4.0 and the XISF 1.0 specification, and
verified against real files written by the applications that produce them. That is the
whole provenance: published specifications for the rules, and real frames for the
evidence that the rules were read correctly.

Four disciplines keep it that way. They are recorded so nobody later mistakes a
self-imposed rule for an external constraint, or relaxes one believing someone else owns
the decision.

1. **No code transliterated from any specification's sample implementation.** The XISF
   specification embeds a C++ reference implementation of the byte-shuffling transform
   alongside its mathematical description (§10.6.2). A specification that ships sample
   code is still someone's copyrighted code. Every algorithm here is implemented from
   the *described* transform.
2. **The normalization convention is measured from output, never read out of an
   implementation.** The form in § Normalization is checkable by anyone holding a file:
   the corpus's `Float32` variants land on exactly the bits the integer path produces
   for the same samples, which is what pins it. Observation of behaviour is not
   transcription of source.
3. **No `pixinsight-*`, `pcl-*`, or `pleiades-*` in crate, module, or binary names.**
   Descriptive factual statements ("reads PixInsight's XISF format") are fine; the lint
   lane greps for the rest.
4. **The converted XISF specification is not redistributed.** `reference/*.md` and
   `reference/*.html` are gitignored with the README re-included; the converter is
   ours and is committed. This document cites the specification by section number and
   does not quote it at length.

**The crate is licensed `MIT OR Apache-2.0`.** That is the Rust ecosystem's norm, it costs
one extra file, and it lets a consumer take whichever half its own obligations prefer —
Apache-2.0's explicit patent grant among them. The dependency graph
does not constrain the choice either way: `quick-xml`, `lz4_flex` and `ruzstd` are MIT-only,
and MIT's terms are satisfiable under Apache-2.0 distribution; every other runtime
dependency is already dual-licensed this way (§ Dependencies). The `deny` lane
(§ Operations) enforces both halves ship.

## Technical Plan

### Layering

The crate is two layers, and the separation is what lets one library serve both a
bit-exact consumer and a general one.

```
                    ┌──────────────────────────────────────────┐
   source bytes ──► │  Layer 1 — CONTAINER                     │
   (impl Read)      │  parse header · locate pixel data        │
                    │  decompress · unshuffle · fix byte order │
                    │  de-interleave                           │
                    │  OUT: native samples (u8/u16/u32/u64,    │
                    │       i16/i32/i64, f32/f64), planar      │
                    └───────────────┬──────────────────────────┘
                                    │
                    ┌───────────────▼──────────────────────────┐
                    │  Layer 2 — NORMALIZATION                 │
                    │  one pure function, pinned bit-for-bit,  │
                    │  identical for every container format    │
                    │  OUT: f32 in [0,1]                       │
                    └──────────────────────────────────────────┘
```

Layer 1 is where the two formats differ and where all the parsing, hardening and
streaming lives. Layer 2 is format-independent by construction, which is what makes the
normalization form apply **identically for every container format** — a rule this design
settles rather than one either specification imposes. The cross-format bit-identity
guarantee is therefore *structural*, not a convention two code paths have to remember to
honor. Layer 2 is also a **pure function of a raw sample, the scaling, and the range**, so
the exhaustive test that pins it needs no file at all.

**The primitive owns every step**: the `f64` scaling, the narrowing cast, the multiply,
the saturating clamp, and the zero normalization. It is a small public value built once
per image from the scaling and the range — the `f32` reciprocal `k` computed at
construction, not per sample — with a per-sample method generic over the native sample
widths and a slice-wise method that is exactly a loop over it. Nothing is left to the
caller: a caller handed an `f64` would already have performed step 1, and the
*Streaming equals whole-buffer* criterion would then be pinning the caller's arithmetic
rather than this crate's.

### The organizing principle: report, don't interpret

One rule generates most of this design:

> **`astroframe` reports what the file says. It applies exactly one transformation —
> the sample-to-`[0,1]` normalization the format specifications define — and it applies
> no other. Policy belongs to the consumer.**

| Thing the file says | What a decoder is tempted to do | What `astroframe` does |
| --- | --- | --- |
| FITS `ROWORDER = 'BOTTOM-UP'` | Flip the rows, or refuse the frame | Report `RowOrder::BottomUp`; deliver rows in stored order |
| XISF `orientation="90;flip"` | Rotate the image | Report it; deliver samples in stored order. §11.5.2 says a decoder claiming orientation support *should* apply the transform for images "to be represented visually" and *must not* apply it to images loaded for processing that depends on the physical disposition of pixels — this crate is squarely the second case |
| FITS `PEDESTAL = 100` | Subtract it, or refuse the frame | Report the keyword; subtract nothing |
| XISF `offset="0.01"` | Subtract it | Report it; subtract nothing. §11.5.2 calls it "also known as *pedestal*" and places the subtraction in calibration and integration processes — i.e. in a consumer |
| A keyword the consumer cares about (`EGAIN`, `RA`, `SITELAT`) | Parse and interpret it | Report the value as text, verbatim |
| A frame outside some consumer's validated envelope | Mark it "unvalidated" | Report the facts (sample format, bounds provenance, scaling) the consumer's predicate needs |

The one transformation it does apply is normalization, because without it there is no
defined pixel value at all — and even that is reachable un-applied, through the
native-sample layer below.

**Saturation is part of that one transformation, and it is lossy — say so plainly.**
Clamping to `[0,1]` destroys information, and the case is not exotic: an XISF `Float32`
frame with the near-universal `bounds="0:1"` normalizes by an identity multiply, so the
clamp is the *only* thing that happens to it, and out-of-range samples — routine after
stacking or deconvolution — flatten to `0.0` or `1.0`. It is accepted for one reason:
the range map is what `[0,1]` output *means*, and XISF §8.5.5 defines it with
saturation. The mitigation is structural rather than a caveat — the unclamped samples
remain exactly recoverable through layer 1. Invariant I3 — no silent transformation — is
worded to include it.

The rule cuts the other way too: `astroframe` **never silently repairs** a file. A
frame it cannot decode under its documented rules is an error, never a best guess.

### Normalization

**This section is the single normative home of the normalization form. Nothing else in
this document restates it; every other mention points here.**

XISF §8.5.5 defines the linear mapping from an image's representable range onto the
output range and is **silent on floating-point rounding**; both a multiply-by-reciprocal
and a divide satisfy it. The authority for *which range maps to which* is §8.5.5. The
authority for *how the product is computed* is **this section**, which pins one form and
holds it by the exhaustive tests § Acceptance Criteria names. Conflating the two would be
the one divergence from a normative source this design failed to declare.

The evidence base is narrower than the guarantee, so the resolution is stated in advance
rather than left to whoever finds out. The form was established by measurement at `UInt16`
and is asserted at every width this crate decodes; no measurement exists of any particular
application's XISF path at any width, and § Licensing boundary forbids obtaining one by
reading source. **If a future measurement shows one application's FITS and XISF paths
differing from each other, invariant I1 wins** — this crate keeps one arithmetic for both
containers, and a consumer wanting to claim parity with a particular application scopes that
claim to the container it measured. A library whose two formats disagree by design is the
defect this document exists to prevent.

Every sample passes through three steps, in this order, with the casts exactly where
they are written. `lo`, `hi`, `bscale` and `bzero` are all `f64`; `k` alone is `f32`.
The operand widths are pinned to the bit because leaving the operand *type* to the
implementer is the larger hole: for a float source with a nonzero declared `lo`, an f32
subtraction differs from the f64-then-narrow one by a ULP.

```text
step 1  physical : f64 = bscale * (raw as f64) + bzero      // only when the source
                                                            // carries FITS scaling
        physical : f64 = raw as f64                         // otherwise (XISF, and
                                                            // FITS with no scaling)
step 2  shifted  : f32 = (physical - lo) as f32             // lo = representable-range low
step 3  out      : f32 = shifted * k                        // k : f32 = 1.0f32 / ((hi - lo) as f32)
```

`k` is computed **once per image**, in `f32`, as the reciprocal of the range width.
This is the multiply-by-rounded-`f32`-reciprocal form this section pins.

Step 1's two spellings differ on no observable output. The only value on which
`1.0 * x + 0.0` differs from `x` is `-0.0`; step 2 then computes `-0.0 - lo`, which
equals `+0.0 - lo` for every `lo`; and step 3 ends by normalizing any zero result to
`+0.0`. Both spellings are kept because the unscaled path is a genuine identity and
writing it as an arithmetic no-op invites someone to "simplify" the *scaled* path to
match — not because a test could tell them apart.

**Why the required form falls out.** For an unsigned integer image at its default
representable range, `lo = 0` and `hi = 2ⁿ − 1`, so step 2 becomes `physical as f32`
(subtracting zero is exact, so the cast is the only rounding) and step 3 becomes
`physical_f32 * (1.0f32 / 65535.0f32)` for a 16-bit image. That is exactly the pinned
expression, reached without a special case — and the equivalence is checked, not merely
argued: the generalized three-step form was evaluated against the reference two-step
form across all 65 536 levels of a `BITPIX = 16`, `BZERO = 32768`, `BSCALE = 1` FITS
image and agrees on every one, bit for bit, with level 0 giving `0x00000000` and full
scale giving `0x3F800000`.

**Why this and not `x as f32 / 65535.0`.** The idiomatic Rust line compiles, looks
cleaner, and is the wrong arithmetic. Division is correctly rounded — one
rounding. The required form has two: the reciprocal is rounded, then the product is
rounded. Measured over all inputs:

| Sample format | Levels where the two forms differ | Max distance |
| --- | --- | --- |
| `UInt8` (`k = 1.0f32/255.0f32`) | **126 of 256 (49.2%)** † | 1 ULP |
| `UInt16` (`k = 1.0f32/65535.0f32`) | 512 of 65536 (0.781%) | 1 ULP |

† The 8-bit row is a first-party measurement made for this document. First differing
8-bit levels: 3, 6, 7, 12, 13, 14.

The first differing 16-bit levels are 257, 261, 265, 269, 273, 277 — at level 257 the
two forms give `3.921568393707e-03` (multiply) and `3.921568859369e-03` (divide). Both
endpoints are exact under the required form: level 0 gives `0.0` and full scale gives
exactly `1.0`. The 16-bit figure is the one that has already caused damage; the 8-bit
figure is starker — **half** of all 8-bit levels differ — and removes any temptation to
treat the rule as a 16-bit curiosity.

Consequences the implementation must carry:

1. `k` is `f32`, computed by reciprocal, never by dividing per sample, and computed
   **from the `f32`-narrowed width**. The obvious warning is the wrong one: once the
   width has been narrowed to `f32`, taking the reciprocal in `f64` and narrowing the
   result is **bit-identical** to taking it in `f32` — verified across `f32`-representable
   widths with zero disagreements. What is not harmless is taking the reciprocal of the
   **un-narrowed `f64`** width, which disagrees on roughly a quarter of widths. The
   `((hi - lo) as f32)` cast is load-bearing, not decorative, and a declared `bounds`
   pair can be any width at all.
2. **No fused or fast-math helper touches this path**: no `mul_add` or any fused helper,
   no `algebraic_*`, no `powi`/`to_degrees`/`to_radians`, and no removal of a cast that
   looks redundant. § Dependencies extends the same ban to the crates beneath this one.
3. **The guarantee is scoped to targets with IEEE-754 `f32`/`f64` arithmetic and no
   excess precision** — every `x86_64-*`, `aarch64-*`, and any 32-bit x86 target with
   SSE2 (which Rust's `i686-*` targets enable by default). It explicitly does **not**
   cover x87-only targets such as `i586-*`, where excess intermediate precision breaks
   the `to_bits()` equality the contract rests on. `wasm32-*` qualifies and is supported
   for finite arithmetic, with one carve-out: WebAssembly leaves a NaN result's payload
   and sign bit nondeterministic, so the NaN half of the *Non-finite handling is total*
   criterion compares with `is_nan()` there rather than `to_bits()`.
4. **The guarantee also assumes the default floating-point environment** —
   round-to-nearest-even, subnormals preserved. LLVM assumes both, Rust exposes no
   stable way to opt out, and a host process that has set FTZ/DAZ in `MXCSR` breaks it
   from outside the crate, which is what an embedding in a non-Rust host would make
   reachable. The crate cannot police the environment; the assumption is stated here and
   repeated in `docs/intentional-patterns.md`.
5. **The decoded bits are part of the public API.** A release that changes one ULP of
   output for an input it previously decoded is a breaking change: at `0.x` that means
   `0.1 → 0.2` and never `0.1.0 → 0.1.1`; after `1.0` a major bump.
6. The line is a magnet for well-meaning simplification, so the argument against
   simplifying it lives in `docs/intentional-patterns.md`, and an exhaustive test fails
   loudly if anyone "cleans it up" anyway. Element-wise autovectorization of the loop is
   permitted and changes no bits — Rust emits no fast-math flags, never contracts
   `a*b+c` into an FMA, and never reassociates. That does not extend to floating-point
   *reductions*, of which there are none on this path and none may be added.

**Saturation and NaN.** §8.5.5's Equation [18] defines the range map with hard
saturation; §8.1 requires infinities and NaN to be "correctly handled (in an
implementation-specific manner)" — a requirement to handle them, with the manner left open
rather than the outcome — and §8.3.3 makes `NaN`, `+Inf` and `-Inf` conforming spellings for
the `bounds` attribute itself, so none of the three is hypothetical. Step 3's result is
therefore defined by exactly one statement:

> A **finite** result saturates into `[0,1]`. **±Infinity** saturates too: `+Inf` to
> `1.0`, `-Inf` to `+0.0`. **NaN** stays NaN.

This is a saturating clamp on ordered values, not a `min`/`max` pair — a `min`/`max`
clamp folds NaN to a bound, which is the outcome being avoided. NaN is preserved rather
than folded to zero because a NaN pixel is a real signal (a masked or dead pixel) and
turning it into black data is quiet fabrication. The cost is that the output invariant
is "finite samples lie in `[0,1]`", not "all samples lie in `[0,1]`". Only float sources
can produce a NaN, so a consumer restricted to integer sources gets the stronger
invariant for free. What a consumer does with a NaN pixel is its own decision and is not
settled here.

**The output never carries a negative zero.** Two paths reach one and both are closed
at step 3: a sample below `lo` produces a negative product that the clamp brings to the
low bound, and a stored `-0.0` sample with `lo = +0.0` passes through steps 1 and 2
unchanged and multiplies to `-0.0`, which the clamp does not touch because `-0.0` is not
*below* `+0.0`. So step 3 finishes by normalizing any zero result to `+0.0`. Step 1's
two spellings preserve the sign of a negative zero through the *scaling*, which keeps
the arithmetic exact; this rule removes it from the *output*, where a stray sign bit
would trip the `to_bits()` comparisons the tests depend on. Preserve internally,
normalize at the boundary.

**Endpoint exactness does not survive a declared range.** For the default integer
ranges the endpoints are exact and that is pinned. For an arbitrary declared `bounds`,
`hi` does **not** generally normalize to `1.0`: `((hi - lo) as f32) * k` depends only on
the width's mantissa, so the answer is exhaustive rather than sampled — over all 2²³
mantissas, **84.6533%** land exactly on `1.0` and **15.3467%** land one ULP low
(`0x3F7FFFFF`). None land high, so the saturating clamp cannot repair it. Every XISF
float frame and every `with_bounds` call is in that regime. This is inherent to
computing a reciprocal and multiplying, which is the form the contract requires; it is
recorded rather than fixed, and it is why the endpoint-exactness claim is scoped to
default integer ranges wherever it appears.

**Precision limits of the normalized output.** `f32` holds 1 065 353 217 distinct
values in `[0,1]`. That separates every `UInt8` and `UInt16` level comfortably, so
normalization is lossless and reversible for those widths. It does **not** separate
`UInt32` (4 294 967 296 levels) or `UInt64`, where distinct stored levels necessarily
collide. Nor does it preserve a `Float64` source, which loses roughly 29 mantissa bits
at step 2's narrowing cast — the *common* lossy case rather than the exotic one: it
covers 216 corpus variants and every `Float64` master, against zero `UInt64` files
anywhere. Endpoints stay exact at every integer width — level 0 gives `+0.0` and full
scale exactly `1.0`, checked for 8, 16, 32 and 64 bits — but a consumer needing exact
levels from a `UInt32` or `UInt64` image must take native samples.

At the **top** of the range this is a property of the `[0,1]` `f32` domain itself, and
no expression avoids it. At the **bottom** of a 64-bit range it is step 1's `f64` that
loses the data first: with `BITPIX = 64` under the unsigned convention, a physical value
of 1 falls off the end of the `f64` addition and reaches `0.0`, where a single
correctly-rounded division would have produced a value `f32` represents comfortably.
That follows from doing the scaling in `f64` at all, which the contract requires. A
related lexing detail: the unsigned-64 convention's `BZERO` is `2⁶³`, which exceeds
`i64::MAX` and can only be parsed as a float, so the keyword-value lexer must not assume
an integer-valued keyword fits an `i64`.

#### Where the representable range comes from

`lo` and `hi` are the only per-format input to the expression above. This table decides
every combination of container and sample-format class this version supports; a source
matching no row is one this version declines (§ Format support matrix).

| Source | `lo`, `hi` | Authority |
| --- | --- | --- |
| XISF integer, no `bounds` attribute | `0`, `2ⁿ − 1` | XISF §8.5.5 default |
| XISF integer **with** `bounds` | the declared pair | XISF §11.5.1 — optional for integer and complex images, and when present it overrides the default |
| XISF `Float32`/`Float64` | the declared pair; `bounds` is **mandatory** | XISF §11.5.1. A float image omitting it is `Malformed` — scoped like an unparseable `bounds` and raised at the same point: the header parses, `bounds()` reports `Unavailable`, native samples decode, and only the normalized output is refused |
| FITS integer `BITPIX`, scaled by the FITS unsigned convention | `0`, `2ⁿ − 1` | FITS unsigned-integer convention |
| FITS integer `BITPIX`, any other `BSCALE`/`BZERO` | **none — no normalized output** | The physical values do not land in `[0, 2ⁿ − 1]`, so there is no range to normalize against without inventing one |
| FITS float `BITPIX` (−32, −64) | **none — no normalized output** | FITS defines no *representable* range for floats. `DATAMIN`/`DATAMAX` are reported as ordinary keywords, not consumed: they describe the range the data *occupies*, not the range it is *displayed against*, and conflating the two would rescale every frame by its own content |
| Any source, caller override | whatever the caller supplies | `Reader::with_bounds(lo, hi)` |

**One validity rule governs every range, however it arrives — and it is a rule about
`k`, not about the endpoints.** Checking the endpoints is the obvious formulation and it
is not sufficient:

| `lo`, `hi` | endpoints finite, `lo < hi`? | `(hi - lo) as f32` | `k` | Every output |
| --- | --- | --- | --- | --- |
| `0`, `1e-46` | yes | `0.0` | `+Inf` | `NaN` across the entire declared range — every in-range sample narrows to `+0.0` and `0 × Inf` is `NaN`; `1.0` appears only outside it |
| `0`, `5e-45` | yes | subnormal | `+Inf` | `NaN` at `lo`; every other in-range sample narrows to a nonzero subnormal, multiplies to `+Inf` and saturates to `1.0` — a uniformly white frame |
| `-1e308`, `1e308` | yes | `+Inf` (already in `f64`) | `+0.0` | `+0.0` at `lo`, `NaN` everywhere else |

All three are reachable from a file-declared `bounds` — §8.3.3 admits any float spelling
— and from `Reader::with_bounds`, and all three produce a frame that looks like a decode
rather than a failure. So the rule is stated on the computed value:

> A range is valid when `k = 1.0f32 / ((hi - lo) as f32)` is **finite, positive and
> normal**.

That is deliberately *not* restated as a condition on the width, because the two are not
equivalent: a width can be finite and normal while `k` is subnormal, for every width
above `2¹²⁶`. Measured — width `2¹²⁶` gives `k = 1.1754943508e-38`, exactly
`f32::MIN_POSITIVE` and therefore the last width the rule **accepts**; the next
representable width gives `1.1754942107e-38`, the largest `f32` subnormal; `1e38` gives
`1.000e-38` and `f32::MAX` gives `2.939e-39`, all subnormal from perfectly normal
widths. Testing the width would admit that whole band and silently lose precision across
it. The rule is knowingly asymmetric the other way: a **subnormal width** whose
reciprocal is normal passes — measured, `lo = 0, hi = 1e-38` gives a subnormal `f32`
width and `k = 1.0e38`. That band costs one to three mantissa bits and is admitted
rather than excluded, because such a range is not a plausible image and the rule's job
is to catch the frames that come out uniformly black, white or NaN. Rejecting non-finite
or reversed endpoints falls out of the same rule.

**The rule applies identically to a file-declared `bounds` and to `with_bounds`, but not
at the same phase and not with the same blast radius.** `bounds` affects only
normalization, so an invalid declared pair does not condemn the file: the header parses,
`bounds()` reports `Unavailable(InvalidDeclared)`, native samples decode as always, and
only `read_image_into` refuses — with `with_bounds` as the same escape hatch the
FITS-float row uses. Rejecting the whole source would contradict the layering.
`with_bounds` returns `Result`, refuses the same values as `InvalidRequest`, and may be
called only before the pixel phase begins. **Its operands are physical values** —
post-`BSCALE`/`BZERO`, the units step 2 works in — so on a `BITPIX = 16`, `BZERO = 32768`
frame the pair that reproduces the default range is `(0, 65535)`, and `(-32768, 32767)`
yields a different image rather than the same one written another way. The units are stated
rather than left to be derived from step 2 because the wrong reading decodes silently.

The two "no normalized output" rows work the same way: such a frame is **not** rejected;
its native samples decode normally through layer 1 and only the normalized output is
refused. A flat rejection would have been the easy answer and a worse one.

**What "the FITS unsigned convention" means, exactly.** Normalized output is offered for
an integer `BITPIX` only when `BSCALE` is 1 and `BZERO` is the value that maps the signed
storage type onto its unsigned range — `0` for `BITPIX = 8`, `32768` for 16,
`2147483648` for 32, `2⁶³` for 64. Those are the cases where physical values provably
occupy `[0, 2ⁿ − 1]`. Any other pairing is refused rather than normalized: a genuinely
signed frame (`BITPIX = 16`, `BZERO = 0`) would otherwise have half its levels saturate
to black, and a rescaled frame (`BSCALE = 0.001`) would normalize to a sliver near zero.
Both would *look* like images and be wrong. Refusing costs the caller one `with_bounds`
call and tells them a decision is needed.

A decoder that enforced `BSCALE = 1` while placing no constraint on `BZERO` would admit a
`BITPIX = 16`, `BZERO = 0` frame and normalize it to pixels spanning `[-0.5, 0.5]` —
outside the `[0,1]` such a contract claims. Constraining both is what closes that.

### The API

Three tiers, in order of how much a caller wants. "Tier" is about how much of the file
a caller asks for; it is unrelated to the two *layers* above, which are about what the
bytes have been turned into.

```
  constructors:  Reader::open(path)        ·  Reader::open_with_limits(path, lim)
                 Reader::sequential(r)     ·  Reader::sequential_with_limits(r, lim)
                 Reader::seekable(r)       ·  Reader::seekable_with_limits(r, lim)
                                 │
                                 ▼
        ┌──────────────────────────────────────────────────┐
        │  Reader<S>                                       │
        │                                                  │
        │  header phase — parsed at construction           │
        │    · header() -> Option<Header>         [tier 1] │
        │        · geometry · keywords · properties        │
        │        · row_order() · granularity()             │
        │    · next_image()                                │
        │                                                  │
        │  configuration — before the pixel phase          │
        │    · with_bounds(lo, hi)                         │
        │    · select_channel(k)                           │
        │                                                  │
        │  pixel phase — on demand                         │
        │    · chunks() -> Chunks                 [tier 3] │
        │    · for_each_chunk(|chunk| ..)         [tier 3] │
        │    · read_samples_into(&mut Samples)    [tier 2] │
        │    · read_image_into(&mut [f32])        [tier 2] │
        │    · read_image() -> Image              [tier 2] │
        └──────────────────────────────────────────────────┘
```

**Every constructor comes in a pair.** The short spelling uses `Limits::default()`; the
`_with_limits` spelling takes the caller's. Limits are fixed for the reader's life, with no
setter, because the header parse the constructor performs is already subject to them — a cap
that could change afterwards would have to describe which of the two settings had governed
the bytes already read. § The caps is their single normative home: the values, both
directions a caller moves them, and the two mechanisms rejected in favour of a parameter.

**Tier 1 — header only.** Constructing a `Reader` parses the first header unit and
stops; no pixel byte is read. For FITS that is the 2880-byte blocks up to `END`; for
XISF the 16-byte preamble plus the declared XML header length. A tool sweeping a
night's frames for pixel scale and timestamps pays only this.

**Tier 2 — whole-image decode into a destination.** `read_image_into(&mut [f32])` fills
a caller-owned buffer, so a batch consumer allocates once and reuses across frames.
`read_samples_into` does the same at layer 1, in the file's own sample type.
`read_image()` is the allocating convenience wrapper. **Tier 2 is implemented on top of
tier 3**, which makes "streamed and whole-buffer decode produce bit-identical buffers"
true by construction rather than by two code paths agreeing. No separate optimized
whole-image path may be added later: two implementations that must agree is precisely
the problem the layering removes, and invariant I2 — delivery does not change bits — would
then rest on a test rather than on structure.

**Tier 3 — chunked delivery**, to a caller-supplied sink. **A chunk is a contiguous run of
one channel's samples, in native form**, carrying the channel index and the sample range it
covers, borrowing the reader's scratch buffer rather than owning anything. Its sample range
is expressed in **destination coordinates** — offsets into the buffer the caller supplied —
so assembling a buffer from chunks is a copy at the stated offset with no recalculation.
That distinction only bites under `select_channel`, where file and destination coordinates
diverge: the range is always the destination's, the channel index always the file's. Chunk
extent is the reader's choice and is independent of `Granularity` — a `WholeImage` source
still delivers chunks, it simply had to read everything before the first one. Callers
wanting normalized `f32` use tier 2, or normalize a chunk themselves with the same public
primitive, which is what makes the *Streaming equals whole-buffer* criterion's bit-identity
assertion meaningful rather than tautological.

Two spellings, one of them a wrapper over the other:

- `chunks()` is the **pull** form: a chunk borrows the reader's scratch buffer, so it is
  a `while let Some(chunk) = chunks.next_chunk()?` loop rather than an `Iterator` — the
  same shape `quick-xml` uses for `read_event_into`, and the idiomatic Rust answer for
  lending iteration.
- `for_each_chunk(|chunk| …)` is the **push** form, implemented by driving the pull
  form. Its callback returns `ControlFlow`, so a caller can stop early without
  inventing an error; a callback needing to fail with its own error keeps it in
  captured state and returns `Break`. Threading a caller error type through the
  decoder would make the signature generic for no gain the pull form does not
  already offer.

The pull form is primary because it composes: a caller can stop early, interleave work,
or keep the reader alive across calls.

**The public type surface.** Three data types carry the payload. `Header` is everything
parsed before pixels. `Image` is a decoded frame — a `Header` plus normalized `f32`,
with `channel(k)` slicing one channel out. `Samples` is the native-sample counterpart:
an enum over exactly the scalar widths the two formats can store — `U8`, `U16`, `U32`,
`U64`, `I16`, `I32`, `I64`, `F32`, `F64` — which is what makes a complex sample format
unrepresentable in this crate's output rather than merely unimplemented. The signed
variants exist for FITS, where `BITPIX` 16, 32 and 64 store *signed* integers;
`BITPIX = 8` is unsigned and XISF has no signed formats at all, so no source can produce
an `I8`. Native samples are what the file holds, so the FITS unsigned convention stays
entirely in layer 2 where `BSCALE`/`BZERO` live. Alongside them the crate exposes
`Error`, `Limits`, the enums the accessor table names, the `Chunks` cursor, and — reachable
independently of any format, which is what lets a features-off build still be useful —
the normalization primitive itself.

**One output type, format-specific machinery, no public trait.** `Header` and `Image`
are shared by both formats — that is the point, since the output is normalized and
format-independent by contract. The *decoders* are format-specific and live behind
`Reader`, which sniffs the leading bytes (`SIMPLE` for FITS, `XISF0100` for XISF) and
dispatches by enum. A trait can be added when a third format arrives without breaking
the shape (§ Alternatives).

**What `Header` carries.** The organizing rule only means something if the reported
facts are reachable, and most of the XISF ones are XML *attributes* — neither
`FITSKeyword` nor `Property` elements, so no keyword lookup reaches them. `Header`
exposes them as typed accessors, format-independent where the two formats agree:

| Group | Accessors | Notes |
| --- | --- | --- |
| Geometry | `width`, `height`, `channels`, `sample_format` | Typed fields, never keyword lookups — an XISF frame need not carry `NAXIS1` at all. All four are `Option`, and the three geometry accessors are reported **as a unit** — all `Some` or all `None`, since a partial geometry is not a state this crate produces. The rule over all four is that `Header` reports `None` for any geometry or sample-format fact whose declared value has no representable form in this crate's model; that is the whole of the collision between a closed output type and `header()` still reporting what it can, and the `None` set is enumerated below |
| Provenance | `bounds()`, `scaling()` | The range actually in force and where it came from; enumerated below |
| Orientation | `row_order()`, `orientation()` | FITS `ROWORDER` and XISF `orientation`, reported, applied to nothing |
| Pedestal | `offset()` | XISF `offset`; the FITS `PEDESTAL` analogue stays a keyword, being a keyword |
| Colour and layout | `color_space()`, `pixel_storage()` | Reported so a caller knows what the channels mean; no conversion is performed |
| Identity | `image_id()`, `image_uuid()`, `image_type()`, `channel_index()` | XISF `id`, `uuid`, `imageType`. Without these a caller stepping through a multi-image file cannot tell which image it holds |
| Delivery | `granularity()` | § Streaming |
| Decodability | `decline_reason()` | `None` for an image this version will decode; `Some` on a declined position, carrying the class and the reason. Without it a batch consumer would have to size a buffer and call `read_image_into` speculatively just to learn a frame is undecodable |
| Metadata | `keywords()`, `properties()` | The two text surfaces |
| Mosaic and display | `cfa()`, `resolution()`, `display_function()` | Reported, never applied; each resolved through `Reference`. All three return `Option`, and all three are `None` on a FITS frame, which defines none of the concepts. On an XISF image `resolution()` and `display_function()` report their specification-defined defaults when the element is absent — 72.0 ppi (§11.11), the identity display function (§11.9) — while `cfa()` has no default, because absence there means the image is not mosaiced |

**Where the four `Option` geometry accessors report `None`.** All four are `Some` at every
position this version will decode — that is, wherever `decline_reason()` is `None`. `None`
appears only at a **declined** position, and only where the declared value has no
representable form. The line is **representability, not validity**: a geometry this crate
can read is reported even when what it reads is what declines the position. So the converse
does not hold, and plenty of declined positions report full geometry — a `BITPIX` that is
missing, unparseable or outside the standard set, a tile-compressed `BINTABLE` reporting the
geometry its `Z*` keywords declare, and the two spellings of an empty axis, FITS
`NAXISn = 0` and an XISF `geometry` naming a zero-length axis. Those last two report alike
because both geometries *read*, which is all `header()` turns on; their classes still
differ, and are settled in § Errors → Where a decline surfaces and in § XISF decisions
respectively. The `None` set is exactly:

| Accessor | Reports `None` at |
| --- | --- |
| `width`, `height`, `channels` | FITS `NAXIS = 1` and `NAXIS > 3`; an XISF `geometry` that cannot be **read** as a width, a height and a channel count — a wrong field count, a non-numeric field, a negative field (the accessors are unsigned, so there is no value to report), or a field beyond the representable width; and any position whose geometry could not be parsed at all |
| `sample_format` | A `BITPIX` that is missing, unparseable or outside the standard set, and a complex XISF sample format |

The axis lengths a declined FITS position does declare stay reachable through `keywords()`,
which reports the structural cards like any others. `Image` runs the other way: it exists
only for a decoded frame, so it exposes plain non-`Option` `width`, `height` and `channels`,
delegating to the `Header` values it is guaranteed to hold, and the common path never pays
for the declined one.

`bounds()` reports one of four states, and the fourth is why the enum cannot collapse:

| State | Meaning |
| --- | --- |
| `Unavailable(reason)` | No usable range exists, and it **carries which** — `NoFormatDefault` for a FITS float frame or FITS integer scaling outside the unsigned convention, `InvalidDeclared` for any image whose declared `bounds` is missing or fails the validity rule. The two raise different classes from `read_image_into` (`Unsupported` and `Malformed`), so a caller holding an `Unavailable` must be able to tell them apart. `InvalidDeclared` applies to **integer** images too |
| `FormatDefault(lo,hi)` | The format's own default applied. It carries the pair, so a tier-3 caller normalizing chunks with the public primitive reads the range directly instead of re-deriving it from the sample width |
| `Declared(lo,hi)` | The file stated this range |
| `CallerSupplied { effective, declared }` | `with_bounds` overrode whatever the file said. `declared` still reports what the file stated — the file's own text verbatim whenever it declared a `bounds` at all, usable or not, and `None` only when it declared none — so an override never erases the evidence |

`scaling()` reports `None` or `Fits { bscale, bzero }`. FITS **always** reports `Fits`,
materializing the `BSCALE = 1`, `BZERO = 0` defaults when the keywords are absent, so
`None` unambiguously means XISF. An accessor whose format does not define the concept
returns `None` rather than a fabricated value — `orientation()` on a FITS frame,
`scaling()` on an XISF one.

**`header()` returns an owned `Option<Header>`, not a borrow.** A borrow would hold
`Reader` immutably while every pixel-phase method needs `&mut self`, so the obvious
usage — read the geometry, size a buffer, decode into it — would not compile, and the
implementer would discover that only after building the whole API. `Header`'s geometry
and enums are trivially copyable; its metadata collections are not, and at the FITS card
cap they are the larger part, so those are `Arc`-shared rather than deep-copied — `Arc`
specifically, not `Rc`, because `Header` and `Image` must stay `Send` for a batch
consumer moving decoded frames between threads. `Image` owns one outright.

**The consumer's envelope predicate, which is what the provenance accessors are for.**
It is short, and `astroframe` does not evaluate it, name it, or ship it — it guarantees
every input to it:

```text
integer_sourced   = sample_format is one of the U* or I* widths
range_undeclared  = bounds() is FormatDefault  // not Unavailable, not Declared
scaling_canonical = scaling() is None, or is the FITS unsigned convention
orientation_plain = row_order() is not BottomUp        // FITS surface
                    and orientation() is None or Identity  // XISF surface

inside_envelope   = all four
```

The orientation clause has **two** terms because the two formats express orientation on
different surfaces, and a predicate testing only `row_order()` would silently admit an
XISF frame declaring `orientation="180"`. `row_order()` is `Option<RowOrder>`: `None`
for XISF, which does not have the concept, with `Unspecified` reserved for a FITS frame
whose keyword is absent. `orientation()` mirrors it — `None` for FITS — over the
values §11.5.2 closes; the wire spellings are `0`, `flip`, `90`, `90;flip`, `-90`,
`-90;flip`, `180`, `180;flip` and this crate's names map in that order: `Identity`,
`Flip`, `Rotate90`, `Rotate90Flip`, `Rotate270`, `Rotate270Flip`, `Rotate180`,
`Rotate180Flip`, plus a text fallback for anything unrecognized. An XISF frame with no
`orientation` attribute reports `Identity` rather than a distinct "absent" state, and
the asymmetry with `RowOrder::Unspecified` is deliberate: `0` and absence mean the same
thing for a spec attribute with a defined default, whereas an absent `ROWORDER`
genuinely differs from a declared `TOP-DOWN` — it is a convention a file may not have
invoked.

Two consequences the consumer owns. An XISF `UInt16` frame carrying an explicit but
redundant `bounds="0:65535"` reports `Declared` and so falls outside the predicate,
though it is numerically identical to the default — legal per §11.5.1, which only says
such a `bounds` *should not* be written, so real writers will produce it. And
`scaling_canonical` is redundant with `range_undeclared` for FITS; it is kept because it
is not redundant for XISF, and a predicate that changes shape by format is worse than one
with a redundant clause.

**Phases, and what resets.** The pixel phase begins at the first call that reads pixel
bytes — `chunks()`, `for_each_chunk`, or any `read_*`. Constructing a `Chunks` cursor is
enough; the boundary is not deferred to the first `next_chunk()`, because a caller
holding a cursor has already committed the reader. `with_bounds` and `select_channel`
are rejected from that point with `InvalidRequest`. Reader state is **per-image**:
`with_bounds` and `select_channel` apply to the currently selected image and are cleared
on `next_image()` — the alternative silently carries a `Float32` image's bounds onto the
`UInt16` image after it, which a multi-image XISF file makes reachable.

**Construction selects no image; `next_image()` advances to the first and to each one
after it**, uniformly across both formats and every file layout:

```text
while reader.next_image()? {
    …decode the current image…
}
```

`header()` returns `None` until the first successful advance, and construction reads only
enough to identify the format and parse the first header unit. Selecting nothing is what
holds across every file layout, including the multi-extension and tile-compressed files
whose primary carries `NAXIS = 0` and which therefore have no first image to select at
all; the two selection models that do not hold, and why, are in § Alternatives. It also
makes the tile-compressed case ordinary: `next_image()` advances to the `ZIMAGE = T`
extension and reports a declined position there. Two consequences, neither guessable: a
single-image source returns `true` then `false`, and the images-per-source cap counts
**advances**, not HDUs — a FITS file with three hundred tables between two images is
nowhere near it.

**`select_channel(k)` narrows the reported geometry.** `channels()` returns `Some(1)`
afterwards and the expected destination length is `width * height` — the header describes
what the reader will *produce*, not what the file happens to hold, so a caller sizing a
buffer from `header()` is right by construction, **provided it fetches the header after
configuring the reader**. `header()` returns an owned value, so a header taken *before*
`select_channel` still describes the file's full channel count and would size a buffer
the narrowed reader then rejects as `InvalidRequest`. Configure, then read the header,
then size. `channel_index()` reports which channel of the file was selected — `None` until
`select_channel` is called, rather than `Some(0)`, so "no selection" and "selected channel
zero" stay distinguishable — and a chunk reports that same file index rather than
renumbering to zero. `Image::channel(k)` indexes the image's *own* channels, so after
`select_channel` the image has one channel and `k` is 0. The two numbering schemes are
deliberate and neither is derivable from the other. At a position whose `channels` is
`None` the call is `InvalidRequest` for every `k`, since there is no channel count for `k`
to be within.

**Narrowing moves the destination, never the file's geometry — and the caps divide along
that line.** The `Total samples` cap counts the **file's** `width × height × channels`
whether or not a channel is selected. It is a geometry sanity check that runs before any
buffer is sized and before any sample width is known (§ The caps, § Errors → Validation
order), which is earlier than the point at which channel selection means anything, and
refusing a hostile declaration that early is the whole of what it is for. The
`Decoded output bytes` cap measures the bytes actually written, so narrowing shrinks it by
construction. `Materialized bytes` is measured at each buffer's own size, so narrowing
shrinks it exactly where the narrowed decode allocates less and not otherwise — a
compressed or checksummed single block is materialized whole however few channels the caller
asked for, since §10.6.1 forbids using any of it before the digest covering all of it is
verified.

**Abandoning a chunk stream leaves the reader positioned mid-image, and that is
recoverable.** Dropping a `Chunks` cursor or returning `Break` ends delivery without
error; `next_image()` then skips whatever remains of the current image's data before
advancing — a forward read-and-discard on a sequential source, a seek on a seekable one,
nothing at all when the block was already fully materialized. So `next_image()` is always
legal after an early stop. Re-reading or re-decoding the *same* image is legal only on a
seekable source; on a sequential one it is `Unsupported`.

**Remaining surface decisions, so an implementer does not have to invent them.**

- `read_samples_into` with a `Samples` variant that does not match the header's
  `sample_format` is `InvalidRequest`, not a silent conversion.
- Destination length is checked with `==`, not `>=`: an oversized slice is as much a
  caller error as an undersized one, and accepting it would leave the tail in an
  unspecified state that the partial-failure rule then has to describe.
- `next_image()` returns `Result<bool>` — `false` at end of source, so end-of-source is
  not an error.
- Every public enum except `Samples` and the borrowed chunk-sample mirror is
  `#[non_exhaustive]`; those two are deliberately closed, for the reason § Deferred and
  out of scope gives.
- `Reader` is `Send` when its source is, and `Sync` when its source is. That every useful
  method takes `&mut self` does not bear on it: `Sync` is about whether sharing a `&T` across
  threads is *sound*, not about whether `&T`'s API does anything useful. `Reader` holds no
  interior mutability, so the auto trait applies whatever its methods take, and forcing it off
  would mean adding a `PhantomData<Cell<()>>` marker for no soundness reason. The honest
  statement is that a shared `&Reader` is safe and useless.
- `Samples` owns its buffers, and `read_samples_into` fills the buffer already inside the
  variant the caller passes — which is what makes "allocate once and reuse across frames"
  true at layer 1 as well as layer 2.
- `for_each_chunk`'s `ControlFlow` carries no break payload: the callback keeps whatever it
  produced in its own captured state, which is also how it returns an error of its own.
- `select_channel` may be called again before the pixel phase begins, and the last call
  wins; it narrows from the *file's* channels each time, not from the previous narrowing.
- `with_bounds` may likewise be called again before the pixel phase begins, and the last
  call wins. A second call is **not** `InvalidRequest`. It sets per-image state that
  `next_image()` clears, per *Phases, and what resets* above — not a builder step — and a
  Rust setter overwrites. Erroring would make a caller track whether it had already called,
  which is state it should not have to keep, and a second call is not ambiguous: the later
  pair is plainly the intended one. Each call is validated on its own against the range
  validity rule § Normalization states, so a rejected second call leaves the first in force
  rather than clearing the range. `bounds()` reports the surviving pair as
  `CallerSupplied`, whose `declared` field still carries what the *file* stated, so
  overriding an override erases no evidence either.
- `offset()` reports §11.5.2's `0` default when the attribute is absent, in the same way
  `resolution()` and `display_function()` report theirs. All three defaults belong to XISF,
  so all three accessors report `None` on a FITS frame rather than the number.

### Streaming

Both formats stream, but not equally, and overselling that is the easy mistake — so the
API says which one you got **before** you decode:

```rust
header.granularity() -> Granularity   // Rows | Block { subblocks: u32 } | WholeImage
```

`Granularity` describes **how much of the input the decoder must hold before it can
produce any sample**. `Block` carries the subblock count, since that is the number of
independently-deliverable pieces and the only thing a caller can act on. It says nothing
about how large a chunk the caller receives, and nothing about where in the destination
those samples land; conflating any of the three is the easy mistake here. At a **declined
position** it reports `WholeImage`, whatever the decline: no delivery is possible there and
every pixel call errors anyway.

Each property of a data block imposes a granularity floor, and the granularity is the
**worst** of them — not the first one found, since they compose:

| Property | Granularity floor | Why |
| --- | --- | --- |
| Codec is `lz4` / `lz4hc` | `Block` | A bare LZ4 block decompresses only as a whole |
| Codec is `zlib`, `zstd`, or none | `Rows` | Both codecs are framed streams and decompress incrementally |
| Byte shuffling (`+sh`) | `Block`, **ignoring `subblocks`** | The shuffle spans the whole pre-split block, so subblock boundaries buy nothing |
| A `checksum` is declared | `Block`, **ignoring `subblocks`** | The digest covers the whole **stored** block (§10.5); `subblocks` splits the compression, not the stored block, so nothing may be delivered until all of it is read and hashed |
| Location is `embedded` | `WholeImage` | The pixels were fully materialized during header parse, so no part of the input remains to stream |

Then one promotion, applying **only to a `Block` floor**: it becomes `WholeImage` unless
the block is split into `subblocks`. Stated on the *deliverable unit* rather than on "the
block", because §11.5 mandates exactly one data block per image — so "the only block
covering the entire image" is always true and a promotion conditioned on it would fire
every time, making `Block` unreachable. A `Rows` floor is never promoted: an
uncompressed or plain-`zlib` single block covering the image still streams by rows, which
is the whole point of those two.

**Every XISF row below is derived from the floors and the promotion above — recompute
them, do not edit them independently.** Only the peak-memory column, and the FITS row
(which follows from § FITS decisions rather than from any block property), carry
information those rules do not.

| Source | Granularity | Peak memory above the destination buffer |
| --- | --- | --- |
| FITS, any `BITPIX`, on an image this version decodes | `Rows` | one I/O buffer (kilobytes) |
| XISF, uncompressed, no checksum | `Rows` | one I/O buffer |
| XISF, `zlib`, no shuffling, no checksum | `Rows` | I/O buffer + inflate window (~32 KiB) |
| XISF, `lz4`/`lz4hc` **+ `subblocks`**, no shuffling, no checksum | `Block` | one subblock, compressed and decompressed |
| XISF, `zlib` **+ `subblocks`**, no shuffling, no checksum | `Rows` | zlib's floor is already `Rows`; subblocks cannot lower it |
| XISF, `subblocks` **+ shuffling** | `WholeImage` | the shuffle floor ignores the subblock split |
| XISF, `subblocks` **+ checksum**, no shuffling | `WholeImage` | the checksum floor ignores the subblock split |
| XISF, checksummed **+ shuffled** **+ subblocked** | `WholeImage` | worst of the floors, not the first |
| XISF, `embedded` | `WholeImage` | pixels already materialized in the header region |
| XISF, one `lz4`/`lz4hc` or checksummed block covering the image | `WholeImage` | the stored block plus its decompressed form |

**Where the time actually goes, measured.** Granularity is a memory property and says
nothing about speed, so the speed answer is recorded separately rather than inferred from it.
Ten corpus files, one per decode shape, single-threaded, 2.68 GB of output in 3.0 s — 900 MB/s
in aggregate, and the spread across shapes is the whole content of the number:

| Shape | MB/s |
| --- | --- |
| Uncompressed `UInt16` / `Float32` | ~6300 |
| `lz4`, without / with a checksum | 2800 / 1990 |
| `zlib`, `Float64` / `Float32` | 1020 / 610 |
| `zstd` | 445 – 485 |

Sampled at 1 ms, self time attributes **≈50% to `miniz_oxide`, ≈33% to `ruzstd`, ≈6% to this
crate**, ≈5% to `memmove` and ≈5% to checksum hashing. So roughly **83% of a compressed decode
is inside the two pure-Rust codec crates**, which § Dependencies chooses deliberately and for a
reason unrelated to speed. That is the ceiling on what optimizing this crate can achieve on
compressed frames.

The shape of the remaining 6% is worth recording, because getting it wrong once cost a
measured 2x. The XISF row decoder originally resolved *per byte* which of three sources a
sample byte came from — a streamed row, a materialized block, or a materialized block needing
the §10.6.2 unshuffle. All three facts are constant for a whole row. Worse, because one helper
served all three, the shuffled arm's codegen governed the other two: giving that arm a
bounds-check panic path slowed the *unshuffled* case by half, on a branch it never executes.
Hoisting the decision to once per row, so the dominant case (unit stride, no shuffle — every
uncompressed, `zlib`, `zstd` and planar frame) reads as one `chunks_exact` the compiler can
vectorize, took the uncompressed shapes from ~2100 MB/s to ~6300 and the row decoder from 14%
of self time to 1%. The lesson generalizes past this function: **a per-byte branch on a
per-row fact is not merely a branch, it is a constraint on how everything around it compiles.**

**Interleaved (`Normal`) storage does not change granularity.** Every input row yields
samples for all channels, so the decoder never has to hold more of the *input*. The cost
is one row-sized staging buffer, not an image-sized one: a chunk is a contiguous run of a
single channel, so interleaved input is transposed a row at a time to produce one.

**`WholeImage` does not mean a second copy of the output.** Tier 2 drives tier 3, and at
`WholeImage` granularity the reader still normalizes *into the caller's destination* —
the buffering the granularity describes is on the input side. A tier-3 caller reading
chunks receives borrowed views over that same work, not a private copy.

The reasons for the floors are structural, not implementation laziness. Byte unshuffling
cannot stream: §10.6.2 stores the byte of significance `j` of sample `i` at offset
`j·N + i` for `N` samples, so reconstructing one sample needs `n` bytes spread across the
entire block and no prefix yields any complete sample. Checksums cover the whole stored
block (§10.5), and §10.6.1 forbids decompressing a block whose checksum failed, so
verification completes before any sample is delivered. And the three codecs have three
different container shapes:

**Dispatch on the declared `compression` name, never on the bytes** — only `zstd` has a
real magic number, so sniffing is not available even in principle. The specification says
nothing about byte-level framing and contains no occurrence of `zstd` at all, so the
corpus is the only authority for two of these three rows; they were established by
reading attachment bytes directly:

| Declared codec | Container shape | Evidence |
| --- | --- | --- |
| `lz4`, `lz4hc` | **Bare block**, no frame header — decompresses only as a whole | The frame magic `04 22 4d 18` is absent; leading bytes are payload |
| `zlib` | **zlib-wrapped**, not raw deflate — feed a zlib reader, not inflate-raw | `78 9c` observed; `78 01`, `78 5e`, `78 da` are equally valid zlib headers |
| `zstd` | **Framed** — the opposite of LZ4, so a streaming decoder applies and the floor is `Rows` | `28 b5 2f fd`, a genuine magic |

Getting any of these wrong fails every block of that codec, and LZ4 and zstd fail in
*opposite* directions: reaching for a framed LZ4 reader breaks LZ4, and reaching for a
bare-block zstd reader breaks zstd. `ruzstd`'s streaming decoder is the right entry point
for the latter.

**`zstd`'s frame header declares a window size, and that is an invariant I4 case — no
allocation from an unvalidated declared size.** A zstd decoder allocates that window before
producing a byte, from a number the file states — the precise shape of "an allocation sized
from an unvalidated declared size". zlib is discharged by its fixed ~32 KiB window and LZ4
has none, so `zstd` is the only codec here that carries one. The declared window is
therefore capped independently, at 8 MiB: far above what any real encoder emits for image
data, far below anything that matters. The adversarial suite gains a **zstd window bomb**
alongside the ported zlib and LZ4 bombs — the ported cases include none, that decoder
having never supported the codec.

Two mitigations are real and are taken. XISF's `subblocks` attribute (§10.6) splits a
block into independently-decompressed pieces, which restores block-granularity streaming
to LZ4 files that use it — `Block` granularity is reachable *only* that way, since `zlib`
and `zstd` already stream by rows and shuffling or a checksum forces `WholeImage`. No
file in the local corpus uses `subblocks` at all, so that row is graded by a hand-built
fixture. And the **destination buffer is always the caller's**, stated in full under
*`WholeImage` does not mean a second copy of the output* above.

#### What streaming is actually worth

Peak decode memory follows from the shape of the path rather than from measurement alone. The
**integer** FITS path holds the raw bytes, a full typed intermediate slice and the `f32`
output at once — 2 + 2 + 4 bytes per pixel — so its peak is about **2.05x** the buffer the
caller asked for. The **float** path is worse, at 4 + 4 + 4 bytes per pixel and about
**3.06x**. Row-streaming removes both leading terms in either case: a halving for integer
frames and better for float ones. That is the concrete target for FITS tier 2, and it is
arithmetic a reader can check rather than a hypothesis, and measurement across real frames
agrees with it. Sensor-class labels and geometry only — the frames themselves are treated as
personal data:

| Sensor class | Format | Geometry | Stored | File | Measured peak | Decoded output¹ |
| --- | --- | --- | --- | --- | --- | --- |
| 26 MP APS-C mono | FITS | 6248 × 4176 × 1 | Int16 | 50 MiB | **206 MiB** | ~100 MiB |
| 26 MP mono, registered | XISF | 6248 × 4176 × 1 | UInt16 | 50 MiB | **156 MiB** | ~100 MiB |
| 61 MP full-frame mono | FITS | 9576 × 6388 × 1 | Int16 | 117 MiB | **474 MiB** | ~233 MiB |
| 26 MP master dark | FITS | 6248 × 4176 × 1 | Float32 | 100 MiB | **306 MiB** | ~100 MiB |

¹ Computed from the geometry, not measured: it is the buffer the caller actually wants —
normalized `f32` for the integer rows, and native `f32` samples for the master dark, which is
a FITS float frame and so has no normalized output under this design.

Row-streaming takes the integer rows from 206 MiB to ~100 MiB and 474 MiB to ~233 MiB, and the
float row from 306 MiB to ~100 MiB. The measured figures carry no compressed FITS and no
multi-channel samples; LZ4-block-compressed XISF payloads are modelled at roughly +0.5–1 byte
per pixel rather than measured.

**The common real-world XISF case is the best case, not the worst.** Every integrated master
checked here carries **no `compression` attribute and no `checksum` attribute** on its image
block, with a declared block size equal to what its geometry implies. That is confirmed by
arithmetic rather than by absence: an uncompressed block is precisely the size its geometry
implies, and every image checked is. By the floors above, those files sit on the **`Rows`**
row, so XISF streams as well as FITS on the files that matter most — and the sizes involved,
hundreds of megabytes per master, make it matter more rather than less.

The honest bottom line, stated in the crate documentation as well as here:

- **Tier 1 is the big win**, and it applies to every format and every codec. A metadata
  sweep over a night's frames stops being a decode at all.
- **FITS tier 2 is the second win**, worth roughly half of peak.
- **XISF tier 2 is a third win**, on the evidence above: real integrated masters are
  uncompressed and unchecksummed, so they stream by rows exactly as FITS does.
- **For a consumer that holds the whole frame resident anyway** — for global statistics, or
  for a multi-pass analysis — streaming does not reduce peak memory below the `f32` buffer
  at all. The win there is avoiding the second copy, not the first.
- **For a compressed single-block XISF frame, streaming still wins nothing.** The
  granularity is `WholeImage` and the API says so up front. That case is real; it is
  simply not the typical one.

### Sources: `Read` versus `Read + Seek`

XISF locates attached blocks by absolute file offset, so reaching one from a
non-seekable source means discarding bytes until the cursor arrives — which works only if
the block lies ahead. Rather than force `Seek` on every caller and lock out pipes and
sockets, the reader is generic over an internal source abstraction with two
implementations:

- **sequential** (`R: Read`) — forward-only; skipping is read-and-discard. A block behind
  the cursor is `Unsupported`, not a silent buffer.
- **seekable** (`R: Read + Seek`) — skipping is a seek; block order does not matter.

`Reader::open(path)` gives a seekable reader. Decoding from an **in-memory buffer** is
`Reader::seekable(Cursor::new(bytes))`; no separate constructor exists, because a `Cursor`
already is a seekable source and a third entry point would only be an alias. FITS never
needs `Seek` in either mode.

### Format support matrix

What v1 decodes and what it declines. Declining is an error a caller can catch and skip
on; it is never a silent fallback.

| | Supported | Declined (`Unsupported`) |
| --- | --- | --- |
| **FITS** | All image HDUs, walked forward: the primary when it holds an image, then each `XTENSION = 'IMAGE'` extension that holds one. An extension declaring `NAXIS = 0` holds no image and is skipped rather than declined, exactly as a `NAXIS = 0` primary is (§ Errors → Where a decline surfaces). `BITPIX` 8/16/32/64 integer. Any `BSCALE`/`BZERO` for native samples. `NAXIS` 2, or 3 read as channels | *For normalized output only*, native samples still decoding: `BITPIX` −32/−64, and any `BSCALE`/`BZERO` outside the FITS unsigned convention. *Outright*, with no pixel output of either kind: `NAXIS = 1`, `NAXIS > 3`, and tile-compressed images — recognized by `ZIMAGE = T` on a `BINTABLE` extension and never by filename, the corpus's tile-compressed files being named `.fits` |
| **XISF** | Monolithic. `UInt8/16/32/64`, `Float32/64`. `Gray` and `RGB`. `Planar` and `Normal` storage. Both byte orders. `attachment` and `embedded` pixel locations. `zlib`, `lz4`, `lz4hc`, all `+sh` variants, `subblocks`; and `zstd`, whose attribute syntax (`zstd:<size>`, `zstd+sh:<size>:<item-size>`) is **corpus-derived rather than specified** — the codec appears nowhere in XISF 1.0. All five checksum algorithms | `Complex32/64`. `CIELab`. Geometry other than `width:height:channels`. Distributed XISF. Signature verification. (`url(...)` and `path(...)` in a monolithic file are `Malformed`, not `Unsupported` — §10.2 forbids external blocks there outright) |
| **Both** | Multi-image sources, advanced forward-only. FITS keywords. XISF `Property` elements whose values are attribute-borne **or character-data** | Writing. CFA/Bayer demosaicing. Colour-space conversion. WCS interpretation. XISF `Property` values stored in **data blocks**; `Table`-typed properties, which §11.1 excludes from `Property` |

**A CFA/OSC frame decodes normally, as a single channel.** Only *demosaicing* is out of
scope — the Bayer mosaic itself is just pixel data, and refusing it would reject a large
share of real amateur frames. How the mosaic is described differs by format and both are
reported: FITS uses convention keywords (`BAYERPAT`, `XBAYROFF`, `YBAYROFF`), which fall
out of ordinary keyword reporting, while XISF carries it in a first-class
`ColorFilterArray` element (§11.10) that no keyword lookup reaches — so `Header` exposes
`cfa()`. Left to the ignore-unknown-elements rule an XISF OSC frame would decode to one
channel with its CFA description silently dropped, which is the failure this design
refuses everywhere else.

**XISF `Property` elements are exposed, not skipped.** §11.5.3 reserves a namespaced set
of astronomical identifiers — `Observation:Time:Start`, `Instrument:ExposureTime` and the
rest — covering the same *subjects* a FITS consumer reads from `DATE-OBS`, `EXPTIME`,
`EGAIN`, `XPIXSZ` and `FOCALLEN`; a consumer wanting a capture timestamp pins the first of
them. The specification states no such mapping and the two surfaces do not agree on units —
`Instrument:Telescope:FocalLength` is metres where FITS `FOCALLEN` is conventionally
millimetres — so reconciling them is a consumer's job, which is the whole reason both
surfaces are reported rather than merged. A frame may carry them as properties, as
`FITSKeyword` elements, as both, or as neither; `astroframe` reports whichever the file
holds and resolves nothing, because precedence is a policy and policies belong to consumers.

A property is reported as a tuple — `id`, `type`, value, and `format`/`comment` when
present (§11.1.1, §11.1.2) — never as a bare string. Dropping `type` would leave a consumer
unable to tell `Observation:Time:Start` as a `TimePoint` from the same identifier spelled
as a `Float64` or a `String`, which is the whole reason a consumer pins that identifier.
The value itself is **verbatim text, never parsed per its `type`** — the rule keyword values
already follow (§ Decisions the implementer must not silently change → Keyword storage), for
the same reason: re-rendering a number through a formatter can lose digits, and the consumer
is the one that parses. `type` is reported so it can.

**`type` is an enum over the specification's vocabulary, with a catch-all.** §11.1.1 makes
the attribute the name of an XISF property type, and that vocabulary is enumerated rather
than open: one variant per type name in Tables 3 through 8 — the scalars of §8.4.4.1, the
complex types of §8.4.4.2, `String` (§8.4.4.3), `TimePoint` (§8.4.4.4), the vectors of
§8.4.4.5 and the matrices of §8.4.4.6 — plus a catch-all carrying the file's own text:

```rust
enum PropertyType { /* one variant per name in Tables 3–8 */ Other(Arc<str>) }
```

Three things decide that shape. The enum is what matches the **specification** instead of
inventing a shape: §8.4.4 has already closed this vocabulary, so a decoder that models it as
free text is declining to read a decision the standard made. `Other` is what keeps the
report **lossless** — report-don't-interpret does not permit silently discarding what the
file said, and a type name this version does not recognize is precisely where discarding is
tempting. And `#[non_exhaustive]`, which follows from the crate-wide rule in § The API
rather than being special here, is what lets a later type name become a variant without a
breaking change. A bare `String` would be simpler and would push stringly-typed comparison
onto every caller instead; classifying a name the specification enumerates is not
interpreting it, so this does not sit against the organizing principle.

Three consequences, none of them guessable from the shape alone. Tables 3 through 8 give
many of those names an **alternate spelling** — `Byte` for `UInt8`, `Complex` for
`Complex64`, `Vector` for `F64Vector`, `Matrix` for `F64Matrix` and the rest — and both
spellings name one type, so both resolve to the one variant rather than the alternate
falling into `Other`; which spelling a writer chose says nothing about the value, and the
value is reported verbatim regardless. There is **no `Table` variant**, because §11.1
excludes table properties from `Property` altogether: an element spelling `type="Table"` is
non-conforming and lands in `Other` like any other unrecognized name, which is the reporting
answer rather than the fatal one, for the reason this section gives throughout. And adding a
variant later reclassifies files that used to land in `Other`, so `Other` is a value to
report and to log — never a stable thing for a consumer to match on.

The vector and matrix variants are reported even though this version does not read those
values: a block-valued `F64Matrix` property is reported with its identifier, its type and
the value `Unavailable`, which is what lets a consumer tell a missing astrometric solution
from one this version cannot read (§ Deferred and out of scope).

Values arrive three ways and two are supported:

| Value location | Supported |
| --- | --- |
| `value` attribute (scalars, `TimePoint`, …) | Yes |
| Character data of the element (strings) | Yes. §11.1.6 says a `String` property ***shall not*** carry a `value` attribute and must serialize its value as character data *or* as a data block, so supporting only attribute-borne values would drop every `Observation:Object:Name`, `Instrument:Camera:Name` and `Processing:History` in existence |
| A data block — `attachment`, `inline:base64`, or the `url(…)`/`path(…)` forms §11.1.6's own examples use | **Reported with the value `Unavailable`**, never silently dropped. The external forms are reported this way rather than raising the `Malformed` that the same spelling raises on an image's **pixel** block: §10.2 forbids external blocks in a monolithic unit, so such a file is non-conforming either way, and the choice is which non-conformance is worth failing a frame over. Pixel data is what the frame *is*; a property is an element that never prevents decoding, and the rule for those is silent non-reporting rather than a fatal error — the identifier, type, format and comment are all in the header and cost nothing, so a consumer can tell "the file does not carry `Processing:History`" from "it does and this version cannot read it". That is the same distinction `bounds()` draws, for the same reason. This is the costliest deferral: on real PixInsight output these carry the entire astrometric solution (§ Deferred and out of scope) |

**Scope is reported, because XISF has three of them** — root, `Image`, and the mandatory
`Metadata` element (§11.4). `properties()` returns the scopes that apply *to the selected
image*, each tagged with where it came from, merged into one document-order list: that
image's own child properties, root-level `Metadata`-scope properties, and root-level ones
reaching it by `Reference` (taking the position of the `Reference` element). Another
image's child properties are **not** included — a consumer pinning `Observation:Time:Start`
must get this frame's timestamp, not the next frame's — and an **unreferenced** root-level
property is not included either, for the same reason an unreferenced root-level
`FITSKeyword` is not: it is attached to no image, and reporting it against an arbitrary
one would invent an association the file does not make. A root-level property attached by
`Reference` is tagged with the scope of the element it is attached *to*, not the root.

**A root-level `Reference` to an `Image` is itself an image occurrence.** §11.13's second
worked example rewrites four identical images as one `<Image uid="…">` plus three bare
root-level `<Reference ref="…"/>`, which it calls achieving "the same result … in a much
cleaner way". Under that spelling the file holds four images, and a decoder walking only
`Image` elements would report one — silently, on a conforming file. So `next_image()`
walks `Image` elements **and** root-level `Reference` elements that resolve to one, in
document order, and the images-per-source cap counts occurrences. Only *root-level* ones: a
`Reference`-to-`Image` appearing **inside** an `<Image>` is not an occurrence and
contributes nothing to that image's keywords or properties — an image is not a metadata
element of another image, and counting it would make the walk depend on nesting. One
consequence has to be stated because it is where this meets the source model: in the
specification's own example all four occurrences share a single attachment offset, so a
**seekable** source re-reads that block per occurrence while a **sequential** source cannot
go back and the second and subsequent occurrences are `Unsupported`.

**Conformance.** XISF §7.2 defines a *baseline decoder* as a short list of concrete
abilities, and this design meets all but one of them fully. The exception is pixel-data
locations: §7.2 says a baseline decoder reads pixel data from inline, embedded and
attachment locations, while §11.5 states an `Image` element cannot serialize pixel data
inline at all. No inline pixel data is read anywhere, so the conformance claim is
qualified rather than asserted; reading an inline-located `Thumbnail` block would close it
and is the obvious way to if it ever matters. Every declined item §7.2 speaks to at all —
`CIELab`, complex formats, `url(…)`/`path(…)`, distributed units, signature verification —
sits *above* baseline, so declining it leaves the decoder conforming rather than partial;
the rest of the Declined column (writing, demosaicing, colour conversion, WCS, block-valued
and `Table` properties) is not a decoder ability §7.2 names. The *Baseline XISF decoder
conformance* criterion grades the claim rather than leaving it asserted.

One asymmetry between the two format columns is correct rather than an omission: **XISF
1.0 has no signed integer sample formats** — Table 11 (§11.5.1) is closed and holds only
the four `UInt*`, two `Float*` and two `Complex*` forms, and there is no XISF analogue of
`BSCALE`/`BZERO`. Signed pixel data is a FITS concern only, handled entirely by the
scaling step.

### Multi-channel images

The decode target is the whole image, planar: `len == width * height * channels`. For a
single-channel frame that is exactly `width * height`, so a consumer reading only those
needs no special case. `Image::channel(k)` slices one channel out. The
three factors are the `Header`'s, and they are `Some` on every position that decodes at all
(§ The API), so the length rule needs no case for a geometry the crate cannot represent —
such a position is declined and every pixel call on it errors.

A caller wanting only one channel of a large RGB frame narrows the reader with
`select_channel(k)`, which drops the other channels' samples instead of materializing
them. How much *reading* that saves depends on the layout **and** on compression: with
uncompressed `Planar` storage the unwanted channels are contiguous runs that can be
skipped outright; with `Normal` storage every sample must still be read and discarded;
and with any compressed single block, or any block carrying a checksum, the whole block is
read regardless — the digest covers all of it and §10.6.1 forbids using any of it before
verification. The memory saving is the same either way; the I/O saving is not, and the API
does not pretend otherwise. No colour-space conversion is performed, so an RGB frame yields
three channels and never a computed intensity — that is a consumer's policy decision, and
an irreversible one.

### Errors

One `#[non_exhaustive]` enum, not per-format errors behind a trait. The consumer's real
need is a two-way split — *skip this frame* versus *abort the run* — and that is a
classification, not a type hierarchy:

| Variant | Meaning | Caller's move |
| --- | --- | --- |
| `Io` | the source failed | **abort** — the next frame will probably fail too |
| `Malformed` | the bytes are not a valid file of this format | skip |
| `Unsupported` | a valid file using a feature this version declines | skip |
| `ChecksumMismatch` | a data block failed verification | skip |
| `LimitExceeded` | a configured cap tripped | skip |
| `InvalidRequest` | the **caller** asked for something impossible | fix the call |

Every variant except `Io` carries a human-readable reason string naming what was expected
and what was found, and where in the file when that is known. For a consumer whose
documented move on `Unsupported` is "skip", the log line *is* the error's value.
`Error::is_io()` expresses the split in one call. Truncation is classified `Malformed`,
not `Io` — a short file is bad data, not a failing disk.

`InvalidRequest` exists deliberately: a wrong-sized destination slice, `select_channel(k)`
beyond the channel count, or `with_bounds` after the pixel phase has begun are caller
errors, not file errors. Without a variant for them the library's only options are a
panic — which the no-panic contract does not cover, since that contract is about
malformed *input* — or misreporting a caller bug as a bad file. Calling either
configuration method a *second* time before the pixel phase is not on that list and is not
an error: both are last-wins, for the reasons § The API gives.

**The line between the three skip-classes matters, because the *Adversarial suite*
criterion asserts error classes and an implementer choosing per call site would make
those assertions unstable:**

- **`Malformed`** — the file contradicts itself or the format. A declared size that
  disagrees with the geometry, a truncated block, unparseable XML.
- **`Unsupported`** — the file is valid and self-consistent, but uses something this
  version declines, or asks something of the *source* it cannot do (an XISF block behind
  the cursor on a sequential source is `Unsupported`, not `Malformed` — the same file
  decodes through `Reader::open`).
- **`LimitExceeded`** — the file is valid and self-consistent, and tripped a configured
  cap.

#### Validation order

**This is the single normative home of the validation order.** Error classes depend on it
— several adversarial fixtures carry an unsupported *attribute* and no `location` at all,
and they yield `Unsupported` only because the location check runs last. Validating
`location` early — the natural instinct, since it drives the read — would reclassify them
as `Malformed`.

XISF header phase, first error wins:

> geometry → `colorSpace` → `sampleFormat` → `byteOrder` → `pixelStorage` → `location`
> → `compression` → `offset`

FITS, per HDU, first error wins — needed for the same reason, since a header can carry two
faults of different classes and the decline table would otherwise not determine which one
the caller sees:

> block and card structure (`END` reached, card grammar, character set) → `SIMPLE`/`XTENSION`
> → `BITPIX` → `NAXIS` and `NAXISn` → `PCOUNT` and `GCOUNT` → `GROUPS` → `ZIMAGE`
> → `BSCALE`/`BZERO`/`BLANK`

So a header carrying both `BITPIX = 5` and `NAXIS = 1` is `Malformed`, not `Unsupported`:
structural validity of a value is settled before scope is. The five sizing keywords sit
where they do because the walk cannot continue without them: a fault in any of them is what
"cannot be sized" means below, and settling them before scope is what keeps that outcome
determinate on a header carrying two faults.

Two checks are deliberately **outside** that order:

| Check | Where it runs | Why |
| --- | --- | --- |
| Total-samples and output-byte caps | **Pixel phase** | The output-byte cap measures a destination that does not exist yet and from which tier 3 is exempt, so a header-phase check could not know which tier the caller will use |
| `bounds` validity (the rule on `k`) | **Evaluated at parse, raised at normalize** | Evaluating during header parse is what lets `bounds()` report `Unavailable(InvalidDeclared)` immediately; raising only at the normalizing call is what lets an unnormalizable frame still parse its header and decode native samples |

**The pixel phase has an order of its own, and it is fixed here too:** the total-samples cap
is evaluated **first**, on the **file's** declared geometry alone — `select_channel` does not
narrow what it counts (§ The caps) — before the declared-size-versus-geometry cross-check and
before the byte caps. It is the only one of the three that needs no sample width (§ The caps),
so it is the only one that can run first. Fixing it there is what makes a frame declaring
an impossible sample count `LimitExceeded` rather than `Malformed` on the size cross-check,
and it is what leaves the rebuilt `geometry overflow` fixture's assertion determinate rather
than arguable either way (*Adversarial suite*).

Both header-phase placements are the same rule § Hardening applies to the
geometry-versus-file-length check: a frame that cannot be *normalized* is not thereby a
frame that cannot be *read*. This order deliberately does **not** match the prior art's,
which checks the caps and `bounds` before `location` (§ Deliberate divergences from prior
art), so ported
error classes do not hold by inheritance — derive each from the class rules above, and see
the *Adversarial suite* criterion for the fixtures the difference forces to be rebuilt.

**Where a decline surfaces.** Every decline has a class and a **place**. There are three,
and the difference is observable:

| Surfaces at | Meaning | Effect on the walk |
| --- | --- | --- |
| **Construction** | The source is not something this crate can read at all | No `Reader` exists; the caller has nothing to walk |
| **`next_image()`** | The source is readable but advancing failed | `Err`, distinct from the `Ok(false)` that means end of source |
| **Declined position** | The reader sits on an image it will not decode | Construction and advancing both succeed; `header()` reports what it can, `decline_reason()` is `Some`, any pixel call is `Err` |

The last column below says what `header()` reports for the geometry three at a declined
position, because "reports what it can" is not the same answer everywhere: § The API fixes
the `None` set, and a row landing on either side of it is a different observable outcome.

| Case | Class | Surfaces at | Geometry reported |
| --- | --- | --- | --- |
| Source matches neither `SIMPLE` nor `XISF0100` | `Malformed` | Construction | — no `Header` |
| XISF signature of another version (`XISF0200`), or root `version` other than `1.0` | `Unsupported` | Construction | — no `Header` |
| XISF unit-level fault: unparseable or oversized XML header, wrong root element, tripped XML guard | `Malformed` (`LimitExceeded` for a guard) | Construction | — no `Header` |
| XISF `embedded` block whose declared digest does not match — its contents are read during header parse, so its checksum is verified there (§ XISF decisions) | `ChecksumMismatch` | Construction | — no `Header` |
| `SIMPLE = F` | `Unsupported` | Construction | — no `Header` |
| FITS header structure fault: card grammar, no `END`, a truncated header block, a byte outside `0x20`–`0x7E` in a keyword-name or value field | `Malformed` | Construction for the primary header, `next_image()` for an extension's | — no `Header` |
| `BITPIX` missing, unparseable, or outside {8, 16, 32, 64, −32, −64} | `Malformed` | Declined position | Full; `sample_format` is `None` |
| `NAXIS` or a `NAXISn` missing or unparseable | `Malformed` | Declined position | `None` |
| `PCOUNT` or `GCOUNT` missing or unparseable in an `IMAGE` extension header | `Malformed` | Declined position | Full |
| `NAXIS = 1`, `NAXIS > 3` | `Unsupported` | Declined position | `None` |
| `NAXISn = 0` (legal FITS, no data) | `Unsupported` | Declined position | Full |
| `GROUPS = T` (random groups, with `NAXIS1 = 0`) | `Unsupported` | Declined position | Full |
| `BINTABLE` with `ZIMAGE = T` (tile-compressed) | `Unsupported` | Declined position | Full, from the `Z*` keywords |
| Any XISF per-`<Image>` attribute fault | per the class rules | Declined position on that image | Full whenever `geometry` reads as a width, a height and a channel count — a zero-length axis included; `None` when it does not |
| Primary `NAXIS = 0`, no image extension found | — | **Neither**: no image is found, `header()` is `None`, and the walk ends normally | — no `Header` |
| `XTENSION = 'IMAGE'` with `NAXIS = 0` | — | **Neither**: not an image position, so it is skipped inside `next_image()` exactly as a non-image extension is | — no `Header` |
| XISF unit whose walk finds no image occurrence | — | **Neither**: the same as a `NAXIS = 0` primary, for the same reason | — no `Header` |
| HDU-traversal cap tripped while advancing | `LimitExceeded` | `next_image()` | — no `Header` |
| A non-image extension whose data unit cannot be sized | `Malformed` | `next_image()` | — no `Header` |
| A skipped data unit whose computed size exceeds the `Skipped block bytes` cap, **on a source that must read to skip** | `LimitExceeded` | `next_image()` | — no `Header` |

A primary with `NAXIS = 0` is not a decline at all — it is the ordinary shape of every
multi-extension file, so only the *absence of any image anywhere* is left, and that is
end-of-source rather than an error. **An `XTENSION = 'IMAGE'` extension with `NAXIS = 0`
answers the same question the same way**: the same header content must not classify
differently for sitting in an extension rather than in the primary, so it is not an image
position and `next_image()` steps over it with the non-image extensions. What follows
from that:

- The skip is exact rather than estimated. §7.1.1 states that an IMAGE extension whose
  `NAXIS` is zero has no data blocks following its header, with `PCOUNT` zero and `GCOUNT`
  one, so it costs no `Skipped block bytes` at all.
- It counts against the HDU-traversal cap like any other skipped extension, and never
  against the images-per-source cap.
- A file made of such extensions holds no image, which is end-of-source rather than a run
  of declined positions.
- It is deliberately distinct from `NAXISn = 0`, which stays a declined `Unsupported`
  position: `NAXIS = 0` declares no data array at all (§7.1.1 adds that there should then
  be no `NAXISn` keywords), while a zero-valued `NAXISn` declares an image with a
  degenerate axis — a position this version declines rather than a non-image.

Most of the table is declined **positions**, and that is the trade behind it: a source may
legitimately hold one image this version cannot read beside three it can, and aborting the
whole walk over the first would be the wrong outcome for a batch consumer. Only a
fault in the source's identity, or in the header region itself, fails at construction — and
the header region is where an `embedded` block's bytes live, which is why a mismatched
digest surfaces there rather than at a pixel call. **A source with no image in it is not a
failure in either format** — a FITS primary with `NAXIS = 0` and no image
extension, and an XISF unit whose header parses and declares none, both walk zero images and
end normally. Both formats answer it the same way deliberately; a decoder whose two formats
disagreed about "this file holds no images" is the inconsistency this design exists to
prevent.

**An HDU whose data unit cannot be sized has no resumption point, so the walk ends there.**
Inventing one misparses every HDU after it. Stepping over any HDU — a declined image or a
non-image extension `next_image()` skips on its way to one — means skipping its data unit
with the full size formula (§ FITS decisions), which needs `BITPIX`, `NAXIS`, every
`NAXISn`, `PCOUNT` and `GCOUNT`; FITS 4.0 §3.4.1 makes all of them mandatory in a conforming
extension header, and a hostile or broken file can make any of them missing, unparseable or
out of the standard set. "Cannot be sized" is exactly that: a missing, unparseable or
out-of-standard-set `BITPIX`; a missing or unparseable `NAXIS`, `NAXISn`, `PCOUNT` or
`GCOUNT`; or size arithmetic that overflows the `u64` the size computation runs in.

The one rule surfaces in two places, and the only difference is whether the caller sees the
position first:

- At an **image position**, the reader sits on it and reports it like any other decline —
  `header()` reports what it can, `decline_reason()` is `Some` — and the **next**
  `next_image()` returns `Err(Malformed)` rather than `Ok(false)` or a guessed skip. An
  unparseable `NAXIS` or `NAXISn` lands here too, with geometry reported as `None`
  (§ The API).
- At a **non-image extension**, the reader never sits on the position, so the failure
  surfaces on the **current** `next_image()`, which returns `Err(Malformed)`.

A size that *computes* and exceeds the `Skipped block bytes` cap is a different outcome —
`LimitExceeded` from `next_image()`, not `Malformed` — because the file is valid and
self-consistent and tripped a configured cap, which is what the three skip-classes above say.
XISF has no equivalent to any of this: its blocks are located by declared offset rather than
by walking past them, so a declined image never blocks the next one.

**A header that did not parse is not a declined position at all**, because there is nothing
to report about it and nothing to skip. That is the row above for FITS card grammar, a
missing `END`, a truncated header block and the character-set faults § Header character set
makes hard errors: `Malformed`, at construction for the primary header and from
`next_image()` for an extension's, ending the walk either way. Those are the FITS-side
fixtures the *Adversarial suite* criterion adds.

**That the same three-way model governs XISF needs saying, because the header is parsed all
at once**: header-phase attribute validation is per-`<Image>`, so a failure in one image
element is a declined position on that image, not a failure of the source. The corpus makes
it concrete — one master holds two images of different geometry *and* different sample
format in the same file. So the ported adversarial
assertions for per-image faults are written against the advance that selects that image, not
against the constructor.

**Two contracts about what a failure leaves behind.** No partial value is ever returned
alongside an error: `read_image()` on failure yields an error and no `Image`, never a
half-filled one. But a **borrowed** destination buffer is left in an unspecified state —
when `read_image_into` or `read_samples_into` fails part-way, the caller's slice may hold
any mixture of decoded and stale data. It is the caller's buffer, so the library neither
zeroes it nor restores it; a caller reusing a buffer across frames must treat a failed
decode as having invalidated it.

**Malformed input never panics.** Any byte sequence produces an `Err`, not an abort —
including the ones that make a naive parser index out of bounds or recurse without bound.
This is a contract, not an aspiration, because a consumer parsing untrusted frames outside
a sandbox depends on it and Rust gives such a caller no cheap way to recover from a panic.
The fuzz targets exist to enforce exactly this.

### Hardening

Both header parsers consume attacker-shaped input, so the guards are part of the design
rather than a later pass. The governing rule, taken from prior art where it is the single
most valuable structural decision:

> **Geometry is the ceiling; every other size the header declares is only ever a
> cross-check.** No allocation is sized from an unvalidated declared size; buffers are
> sized from validated geometry, and geometry is bounded by the caps.

Stated precisely, because the looser version overclaims: geometry is itself
header-declared, so a hostile hundred-byte FITS header announcing
`NAXIS1 = NAXIS2 = 32768` does drive a one-gigabyte allocation before the short read is
discovered. The caps are what bound that, not the rule — which is why the sample and
output-byte caps are load-bearing rather than belt-and-braces.

**On a source of known length, geometry gets a second and much tighter bound.**
`Reader::open` and `Reader::seekable` know the source length, and an uncompressed image
cannot need more stored bytes than the file contains: a 53 MB file declaring 61
megapixels of `UInt16` is rejected as a *sizing input*, before any allocation, without
reading a pixel. It cannot apply to a sequential source, which has no length, so the caps
remain the floor and this is an additional check rather than a replacement.

**It runs in the pixel phase, never during header decode**, and that placement is
load-bearing. A size-capped *prefix* of a file handed to
`Reader::seekable(Cursor::new(prefix))` has exactly the shape this check rejects — known
length, geometry that cannot fit — and refusing it would break header-only decode for a
consumer that fetches only a prefix, which is the whole point of tier 1. So a `Reader` over
a truncated prefix parses its header and reports geometry happily; the mismatch is an error
only when someone asks for pixels that are not there. The corollary is a rule in its own
right: **a declared block offset is never validated during the header phase.**

A declared uncompressed size that disagrees with the geometry-implied size is a
`Malformed` error raised *before* any buffer is allocated, which makes decompression bombs
structurally impossible rather than merely caught. Decompression additionally inflates to
exactly the expected length and then requires end-of-stream, so a bomb is refused after a
few dozen bytes rather than after materializing.

The one deliberate exception to the rule is the XML header itself, which has no geometry
to be checked against. It is read **incrementally up to its cap** rather than pre-allocated
from its declared length, so a declared size still never becomes an allocation.

**The XML guards.** `quick-xml` is a pull parser and gives none of these for free, so all
of them are explicit:

| Guard | Rule |
| --- | --- |
| Declared header length | Rejected above the cap **before** the read |
| `DOCTYPE` | Rejected outright. No legitimate XISF header carries one, and refusing it removes DTDs, entity declarations and XXE as a category in one rule |
| Entity expansion | Cannot amplify. `quick-xml`'s default resolver handles only the five predefined XML entities and does not resolve recursively, and its DTD handling *skips* the internal subset rather than processing it, so declared entities are never defined and billion-laughs is not expressible. Both properties were checked against the pinned version's source; a test pins them, because they belong to the dependency rather than to this crate |
| Element nesting depth | Capped. A parser that recurses over unknown subtrees rather than skipping them iteratively can be made to overflow its stack, which is an abort rather than an error and breaks the no-panic contract |
| Attribute count per element, attribute-value length | Capped. The subblock list earns its own checks precisely because the whole list is one attribute string rather than elements; that reasoning generalizes to one element carrying a million attributes or a single 8 MiB attribute value |
| Total element count | Capped. Without it a header full of `FITSKeyword` elements allocates a struct per element up to the header cap |

#### The caps

**This is the single normative home of the caps.** They are a **per-`Reader` `Limits`
parameter**, defaulted to the values in the table below, and tripping one is
`LimitExceeded`. Each caller supplies its own at construction and nobody's choice leaks
into anybody else's: `Limits` is an argument, so it does not unify across a dependency
graph and there is no global for a second consumer to overwrite.

**Both directions are real, which is why the knob exists at all.** A service accepting
uploads wants the caps *tighter* than the defaults — refusing far below 2³⁰ over untrusted
prefixes is its shape. A workstation tool wants the output-byte cap
*raised*: 2³⁰ bytes is 268 megapixels at `f32`, so an ordinary 3× drizzle of a 61 MP frame
(549 megapixels, 2.2 GB) is refused by the default. Neither consumer is served by a
compile-time constant, whose only escape is a fork.

The defaults themselves do not move for that. They are set against the decode-time threat
model — what an unattended process should absorb from a file it has not vetted — and a
caller that knows its input is not a threat says so at its own call site.

Two mechanisms for the same knob are rejected, both because they let one consumer's choice
reach another's:

| Rejected mechanism | Why |
| --- | --- |
| **A cargo feature** (`large-frames` and the like) | Features unify across the whole dependency graph and the union takes the **loosest** value, so a feature any crate in a build enables silently raises the caps for every other consumer in it — including a header-only consumer parsing untrusted bytes that never asked. It is invisible to the consumer it harms, which is strictly worse than a knob written at a call site |
| **A library-global initializer** (`set_limits()` at startup) | The same hazard moved to runtime: one process, two consumers, last writer wins. It adds an ordering trap the per-`Reader` form does not have — a `Reader` constructed before the initializer runs gets different limits from one constructed after, and neither call site says so |

`Limits` is a plain `Copy` struct with one public field per row below — two for the
stored-block row, whose value is a multiple of the geometry-implied size *and* a floor —
`#[non_exhaustive]` so a later cap is an addition rather than a break, built from
`Limits::default()` and mutated. Its one real cost is accepted rather than argued away: a
caller can set a cap high enough to offer no protection. That appears in the caller's own
source, which is the difference between this and the two rejected mechanisms.

| Cap | Applies to | Default | Bounds |
| --- | --- | --- | --- |
| XML header length | XISF | 8 MiB | The declared header, read incrementally |
| Element nesting depth | XISF | 64 | Parser stack |
| Total element count | XISF | 100 000 | Per-element allocation |
| Subblock count | XISF | 4096 | Subblock list parsing |
| Attributes per element | XISF | 4096 | Per-attribute parser allocation |
| Attribute value length | XISF | 1 MiB | A single attribute's text |
| Assembled keyword value | both | 1 MiB | A `CONTINUE` chain's **assembled** value, checked as it grows. The only cap here that bounds a value against how it was *reached* rather than how it was written: §4.2.1.2 exists to assemble a long string from short records, and XISF lets a `<Reference>` reach one continuation record many times, so 2048 references to one 500 KB record assemble a gigabyte from a 553 KB header — 7590× the input, at tier 1. No sharing closes *that*, the assembled string genuinely being that long, which is what makes a cap the answer for it. **The cap bounds one assembled value and nothing about how many get assembled**: reasoning about references to *one* image, without multiplying by `Images per source`, misses 256 distinct images each assembling their own megabyte — 1.03 GB from a 1.04 MB header, every count inside its own cap. That is a *product of two caps*, which no single cap can bound, and it is closed by sharing the assembly across images rather than by moving this number. The rule the two answers divide on: a cap bounds one thing's size, sharing bounds how many times it is built, and a defect that multiplies two caps together needs whichever of the two applies to the axis that is growing. One mebibyte because a single assembled value longer than the whole of one attribute is already extraordinary, while the long strings the convention exists for — a WCS solution, a `HISTORY` trail — are kilobytes. FITS reaches it only in principle: 4096 cards of 70 bytes bound a chain to about 287 KB, linear in the input, so the cap is a backstop there and the live guard on the XISF side |
| Stored block bytes | XISF | geometry-implied size × 2, floored at 1 MiB | The compressed block read from `attachment:pos:size` |
| zstd decoder window | XISF | 8 MiB | The window the zstd frame header declares |
| FITS header length | FITS | 8 MiB | 2880-byte blocks read while looking for `END` |
| Header card count | FITS | 4096 | Per-keyword allocation |
| HDUs traversed per advance | FITS | 4096 | Every extension skipped inside one `next_image()` — non-image extensions and `IMAGE` extensions declaring `NAXIS = 0` alike, both being positions the walk steps over rather than sits on |
| Total samples | both | 2³⁰ | The **file's** `width × height × channels`, counted whether or not `select_channel` narrowed the reader, and checked before any buffer is sized and before any sample width is known |
| Decoded output bytes | both | 2³⁰ | The destination buffer, measured in the bytes actually written to it — so `select_channel` narrows it |
| Materialized bytes | both | 2³⁰ | Any whole-image or row-shaped buffer this crate allocates for itself, measured at that buffer's own size. This is what bounds tier 3, which has no destination |
| Skipped block bytes | both | 2³⁰ | A block or data unit read-and-discarded to step over it — a thumbnail, the remainder of an abandoned image, or a non-image extension's data unit walked past inside `next_image()`, whose `PCOUNT` heap can be enormous and which a length-unknown source gives nothing else to bound. Bounds a *read*, not an allocation — so it does not apply where skipping is a seek. On a seekable source the cursor moves without transferring bytes and the cap is not consulted; an ordinary MEF whose `BINTABLE` heap exceeds it is walked past, not declined |
| Images per source | both | 256 | `next_image()` advances. Inherited from an XISF decoder, where multi-image is rare; FITS MEF is where it binds, and a mosaic instrument writing more than 256 image extensions is `LimitExceeded` on a valid file — the FITS-side shape most likely to move this number |

Some of these need their reasoning attached, because the obvious reading gets them wrong.

**The stored-block cap closes the one hole in I4.** A declared `attachment:pos:size`
becomes an allocation, because an LZ4 block must be fully resident before it decompresses
— and a compressed block may legitimately exceed its uncompressed size, so the geometry
cross-check alone cannot bound it. On a seekable source the file length bounds it
incidentally; on `Reader::sequential(r)` over a pipe there is no file length at all, which
is exactly the shape a pipe-fed consumer has, and prior art has the identical hole. Two
times the geometry-implied size is far above any real compressor's worst case and far
below anything dangerous.

**The HDU-traversal cap closes the one place I5 could be violated by design rather than by
mistake.** `next_image()` skips every extension that is not an image position — non-image
extensions and `IMAGE` extensions declaring `NAXIS = 0` alike — and that skip is a loop over
header parses and, where there is a data unit at all, data-unit reads: each iteration
bounded individually by the header-length, card and skipped-block caps, but unbounded in
*number*. Over a pipe there is no file length to terminate it, which is a hang, and a hang
is what I5 names alongside panics and unbounded allocation. A finite file makes it merely
slow: a 1 GB source of bare headers is on the order of 350 000 header parses inside a single
call. The fuzzer cannot find this, because fuzz inputs are finite by construction — which is
exactly why it needs a cap rather than a test.

**The two FITS caps are not decoration** — but the card cap only earns its place at a sane
value. At 100 000 cards — 8 000 000 bytes against the 8 MiB byte cap, within 5% of it — it
would be pure duplication; real FITS headers run to tens of cards, so 4096 is the one that
actually binds and catches a header of repeated cards long before the byte cap does. FITS
has no declared header length to bound it, and over a pipe there is no file length either,
so without these two the keyword list grows until the process dies — breaking I4 and I5 for
the `fits_header` fuzz target and for any consumer reading untrusted bytes. The XISF caps
do not cover it: they are format-specific, and the table says which apply where for exactly
that reason.

**The sample and byte caps are not independent, and the document should not pretend
otherwise.** The byte cap measures the **normalized destination** (`samples × 4`), so it
binds at 2²⁸ samples and the 2³⁰ sample cap can never trip first for normalized output.
The sample cap earns its place as the check that runs on geometry alone, before any sample
width is known and therefore before either byte cap can be evaluated at all.

**That placement also settles which channels each of the three counts, and they do not
agree.** The sample cap counts the **file's** channels, because it runs before channel
selection is meaningful and its job is to reject a hostile declaration on the geometry the
file states. The output-byte cap counts only what is written, so `select_channel` narrows
it. `Materialized bytes` counts each buffer at its own size, which narrowing shrinks only
where the narrowed decode allocates less — not for a compressed or checksummed single
block, which is materialized whole. So narrowing never relaxes the sample cap: a
three-channel frame declaring more than 2³⁰ samples is `LimitExceeded` on the geometry it
declares, not on the one channel a caller asked for. It delays the byte caps instead, and
far enough to invert the ordering above: the claim that the sample cap can never trip first
for normalized output holds for a decode of every channel, and a narrowed decode of a cube
of five or more planes is where it stops holding, the destination having shrunk by the
channel factor while the counted geometry did not. That is the intended outcome rather than
a hole — the sample cap is the check on what the file declared — but an implementer reading
the two caps as one ordering would get it wrong.

The byte cap governs **every tier-2 entry point, whether the destination is the caller's or
this crate's**: the distinction that matters is not who allocates but whether a whole image
is materialized at all, and exempting a caller-supplied slice would let the cap be
sidestepped by choosing a different method. Tier 3 is exempt from the *destination* cap,
having no destination, but **not from a bound** — at `WholeImage` granularity the scratch
buffer *is* the whole decompressed image, so a tier-3 caller would otherwise drive the same
allocation tier 2 refuses on the same file. The sample cap alone does not close that: 2³⁰
samples of `Float64` is 8 GiB of scratch against the 1 GiB tier 2 refuses, so a byte bound
is required and a sample bound is not a substitute for it. **The `Materialized bytes` cap
therefore governs every buffer this crate allocates for itself** — the whole-image scratch
at `WholeImage` granularity and the row-shaped staging buffer alike, the latter because
nothing forbids a 2³⁰-sample image being a single row. It is evaluated against **each such
buffer's own size**, before that buffer is sized: geometry × the stored sample width for the
whole-image scratch, one row's width × channels × the stored sample width for the staging
buffer. So a `Rows`-granularity decode of a frame whose whole-image size exceeds the cap is
not refused — streaming it is the point — and what the cap refuses is one allocation that
reaches it.

For wide sample types the byte cap binds first on the allocating paths: a three-channel
61 MP `Float64` frame is roughly 1.5 GB of native samples and trips `LimitExceeded` on a
perfectly valid file. The mundane case matters more: 2³⁰ output bytes is 268 megapixels of
`f32`, so a 3× drizzle of a 61 MP frame (549 Mpx) is refused and a 2× drizzle (244 Mpx)
barely fits. Integrated and drizzled masters are ordinary products, so the default is a
**statement about what an unvetted file may be allowed to cost, not about which images are
legitimate**. A caller decoding frames it produced itself raises `decoded_output_bytes` and
`materialized_bytes` in its own `Limits`; this is the forcing case the parameter exists for.

The format-neutral caps are the ones a header-only cap cannot supply; three of them are
inherited from prior art (`maxSamples`, `maxDecodedBytes`, `maxImages`), whose values sit
deliberately below `i32::MAX`. Every size computation is done in `u64` with
checked arithmetic and narrowed exactly once, at the allocation site, after the relevant cap
has been enforced. **Because a caller may raise a cap past what a 32-bit target can
address, that narrowing is checked rather than infallible**: a `u64` that does not fit
`usize` is `LimitExceeded`, the same class the cap itself raises. The defaults keep it
unreachable, which is what their sub-`i32::MAX` values buy; the drizzle case above makes it
reachable on purpose, since 549 megapixels of `f32` is 2.2 GB and fits no 32-bit `usize`.

Provenance, stated precisely because it is easy to overstate. The 8 MiB figure is a shared
FITS+XISF *file-prefix* cap, chosen to sit under the 16 MiB header ceiling a prior XISF
decoder sets. The depth cap is a **security margin** rather than a measured bound, set
deliberately far below the nesting a general-purpose XML parser permits. The element-count
cap's *value* is carried over with it; only the rationale differs, since at that figure it
sits an order of magnitude *above* any depth bound and limits an axis depth does not
reach. All are defaults a later measurement may move.

The header cap is worth sanity-checking against real files rather than intuition, in two
directions.
Real XML headers are not small: the corpus masters measure 71 KB, 76 KB, 808 KB, 963 KB
and **1.14 MB**, the large ones because an astrometric solution is carried inline — so the
8 MiB cap has roughly 7× headroom over observed output, not the orders of magnitude a
"headers are kilobytes" intuition suggests, and the axis that grows is the one most likely
to keep growing. And **the header cap is where the `embedded` location actually lives**:
for it the pixel data *is* inside the header as Base64 or Base16 text, so 8 MiB of header
admits roughly 6 MB of decoded pixels — about three megapixels at `UInt16`. That is the
real bound on that location, and it is why tier 1's "reads no pixel byte" is true but
uninformative for such files: the pixels are in the region tier 1 reads.

**What the caps bound jointly, stated as a number, because a table of individual caps does not
give a consumer one.** The largest allocation this crate can be made to perform from a header
alone, with every cap at its default, is about **1.05 GB — from an input of about 2.2 MB**. It
is reached by a header whose `<Reference>` elements compose a distinct `CONTINUE` chain for
each of 256 images. `Images per source × Assembled keyword value` is 256 MiB of assembled text;
the intermediate copies assembly makes on the way there are what carry the peak to a gigabyte,
and the figure is measured rather than derived from the two caps alone; § Fuzzing derives it
and records why it is accepted rather than closed. Every count in such a header is inside
its own cap, which is the point: **caps multiply, and the product is the figure a caller
sizing a process against hostile input needs.**

A caller who reads files from a source that did not produce them should lower `Limits`
accordingly rather than rely on the defaults. `Assembled keyword value` is the effective knob —
the product is linear in it, and dropping it to 64 KiB takes the ceiling to about 64 MB while
leaving the long strings the `CONTINUE` convention exists for, which are kilobytes, entirely
intact. Lowering `Images per source` works too and costs more: it refuses valid mosaics.

The crate sets `#![forbid(unsafe_code)]` (the *No `unsafe`* criterion).

#### Fuzzing

The guards above are claims about *all* inputs, and hand-written cases only ever check the
inputs someone thought of. Fuzzing is how those claims are held, so it is specified here
rather than left as a testing detail. The targets are chosen so that every stage handling
an attacker-controlled length is reachable — header parsing alone would leave the size
arithmetic untested:

| Target | Input | What it reaches |
| --- | --- | --- |
| `fits_header` | arbitrary bytes | FITS block and card parsing, value lexing, the character-set rule |
| `xisf_header` | arbitrary bytes | preamble, XML header, the XML guards, attribute parsing |
| `xisf_block` | a header plus a block body | decompression, unshuffle, subblock splitting, checksum verification — where the size arithmetic and the allocation guards live |
| `decode_any` | arbitrary bytes | format sniffing through to a full decode. The destination is sized **from the parsed header**, bounded by the caps — not a fixed capacity, which would make almost every fuzzer-generated geometry an `InvalidRequest` before a pixel byte was touched and reduce the target to a header parser |
| `decode_sequential` | arbitrary bytes, wrapped `Read`-only | The forward-only source: read-and-discard skipping, the backwards-block `Unsupported` rule, thumbnail skipping, and the no-known-length branch of every size check. All the other targets hand the reader a `Cursor`, which is seekable, so this path — a pipe-fed consumer's shape, and the one the FITS and stored-block caps exist for — is otherwise entirely unfuzzed |
| `decode_alloc` | arbitrary bytes | the allocating `read_image()` path, which `decode_any` cannot reach — this is the target that exercises geometry-driven allocation, and its oracle bounds allocation against the geometry-implied size |

**The oracle is not merely "it didn't crash".** Each target asserts that the call returns
rather than panicking or aborting, that it terminates, and that **total allocation stays
under `ALLOC_MULTIPLE × input_length + header_cap`, where `ALLOC_MULTIPLE = 32`** — and for
`decode_alloc` the geometry-implied size on top of that, it being the target whose whole
purpose is geometry-driven allocation. The caps are deliberately *not* the bound, since
summing them would admit a gigabyte of allocation from a twenty-byte input and assert almost
nothing. Two notes on that oracle: measuring total allocation needs a counting global
allocator, which needs `unsafe impl` — that lives in the fuzz crate, not the library, so
`#![forbid(unsafe_code)]` is unaffected; and the bound is expressed against input length
*plus* the caps because a twenty-byte input may legitimately declare a header the reader
grows toward.

**Where `ALLOC_MULTIPLE` comes from.** It began at 8, and 8 covered one phase only. On a
seekable source the geometry-versus-length check already bounds an uncompressed image by
the bytes the input contains (§ Hardening), so the widest legitimate expansion left on
every target but `decode_alloc` is a `UInt8` frame normalized to `f32` — exactly 4×, with
the native samples beneath it bounded by the input length. Eight is that figure with one
doubling of slack. The general bound carries no room for decompression or for a large image
because the target that needs that room, `decode_alloc`, carries its own geometry-implied
term instead. One interaction follows from the derivation and is stated so it is not met as
a surprise: the 4× ceiling rests on the input length bounding the stored image, which a
**compressed** block breaks, its decompressed size being bounded by the caps rather than by
the input. Where that bites, the knob is the `Limits` the fuzz crate passes, not
`ALLOC_MULTIPLE`.

**Changing `ALLOC_MULTIPLE` is not tuning, and the conditions run one way only.** The
implementer may adjust it, subject to all three of:

- **A failure at the current `ALLOC_MULTIPLE` is presumed to be a bug in the allocation,
  not a bad bound.** That presumption is most of what the number is for. A failing run is
  evidence about the code until someone shows otherwise, and the burden of showing
  otherwise sits with whoever wants the bound raised.
- **Raising `ALLOC_MULTIPLE` requires naming the buffer that allocated and showing why that
  allocation is legitimate.** A general argument — that the bound seemed too tight, that
  some headroom is prudent, that the failure looked unrelated — is not a justification. Name
  the buffer, or the bound stands.
- **The justification is recorded in this document**, beside this paragraph. Not in a commit
  message, where the next person to reach for the number will not find it, and where a
  sequence of raises leaves no single place that shows it was a sequence.

**Raised once, from 8 to 32, and this is the record the third condition requires.** The
derivation above covers the pixel path and nothing else. Header parsing's cost is not per byte
but per **element** — a DOM node, its attribute list, the metadata the element materializes,
and the amortized growth of the vectors holding them — and that was never in the 4×. Measured,
60 000 elements of each shape, construction only:

| element | source bytes | allocated per element | × input |
| --- | --- | --- | --- |
| `<A/>` | 4 | 103 | **25.8** |
| `<Reference ref="k"/>` | 20 | 354 | 17.7 |
| `<FITSKeyword name=… value=… comment=…/>` | 44 | 412 | 9.4 |

The worst legal shape is the *smallest* element, not the most attribute-laden one: per-node
overhead is fixed and the source text it is charged against shrinks to four bytes. 25.8× is the
figure the number has to cover, and 32 is that with 1.24× of headroom.

That figure is what it is because the reduction was taken rather than argued away. The node
arena was grown by doubling, and the oracle measures *cumulative* allocation, so every
reallocation copy counted: the same shape measured **32.3× — above the multiple** — until the
arena was sized in one linear pass over the header's `<` count, clamped to the element cap.
The subblock list was sized the same way for the same reason. Neither is an allocation from a
declared size (invariant I4): both figures come from bytes the source actually produced.

**Per-byte headroom and *reachable* headroom are different numbers, and it is the second that
says whether the oracle still asserts anything.** The ratio above never binds on its own,
because `Total element count` caps a header at 100 000 elements: the worst reachable header of
that shape is about 400 kB of input, where the bound's fixed 8 MiB term still dominates.
Measured at the cap — 99 000 bare elements — the header costs **10.2 MB against a 21.1 MB
bound**, 2.06× of room, and `tests/header_alloc.rs` pins exactly that shape so the derivation
cannot go stale unnoticed. The bound also still catches the failure *class*: the four
allocation defects this crate has actually had ran at 2080×, 994×, 262× and 127× input, every
one of them an order of magnitude clear.

One **further** reduction is known, costed and not taken: `xml::Node` is 80 bytes (a
`Span`, a `Vec` of attribute pairs, a `String`, a `Vec` of child indices) and the standard
flattening — text as a span, attributes and children as ranges into two flat side-arrays —
takes it to 32 and removes two heap allocations per element, roughly halving the per-element
figure above. It is not taken because the fixes that preceded this raise had already moved
the class from memory-exhaustion to bounded, and a parser rewrite for headroom is a worse
trade at this point than recording the option. Whoever wants the multiple back below 32
should spend it there first; that is what the first condition asks of them.

The other two conditions were met rather than waived. The presumption that a failure is a bug
in the allocation was honoured first and was most of the work: interning the XML strings into
one arena, packing a keyword's three texts into one allocation, not building occurrences past
`Images per source`, sharing a keyword reached through many references, sharing root
`Metadata` rather than cloning it per image, and assembling a `CONTINUE` chain once rather than
rebuilding it per record. Those closed three real memory-exhaustion vectors — 2.08 GB, 262 MB
and 465 MB from headers of a megabyte or less — before the number moved at all. And the
buffers that remain are named: the XML document's per-node cost, and the per-image keyword
list, one `Keyword` per occurrence, which § The API requires be reported verbatim and
reachable.

The hazard is not one wrong number. It is a bound that ratchets loose one
individually-reasonable step at a time — each raise justified on its own, none of them
looking like the moment the oracle stopped asserting anything, and the requirement to
justify being exactly what makes every step feel principled. An implementer who has just
written a short paragraph explaining a raise has, by that act, made the raise feel earned.
Writing that down is what makes the tenth one visible as the tenth.

**Every target constructs its `Reader` with a `Limits` far tighter than the defaults** —
**2¹⁸ total samples (about a quarter of a megapixel), 2²⁰ decoded output bytes (1 MiB)**
rather than 2³⁰ of each. Small limits are what a fuzz target should use whatever the oracle
asserts: many fast iterations exploring parser state space find more than a few large ones
exercising allocation volume, and large-geometry behaviour belongs to the caps tests and the
local corpus rather than to the fuzzer. Without them a twenty-byte input declaring 2²⁸
samples allocates a gigabyte and *passes*, which libFuzzer's default RSS limit and any
realistic corpus throughput make unworkable. This is § The caps being tightened by its first
caller rather than a fuzz-only constant: the fuzz crate passes a `Limits` like anyone else,
the shipped defaults are unchanged, and the adversarial suite covers the boundary between
the two settings.

They are also what keeps the oracle's bound honest. At 2¹⁸ samples the `f32` destination is
1 MiB, so the 8 MiB header-cap term dominates the bound and it holds with room even on the
compressed path, where the decompressed size is bounded by the caps rather than by the input
length. The pair is one number in two units — 2¹⁸ samples at `f32` is exactly 2²⁰ bytes — so
neither of the two trips before the other on normalized output. Only these two caps move,
because the other fuzz-reachable buffers follow the same geometry rather than needing their
own tightening: at 2¹⁸ samples the whole-image scratch is at most 2 MiB for the widest sample
type and the stored block at most 4 MiB, that cap being the geometry-implied size × 2. Both
sit under the header-cap term on their own, and a seed reaching both at once has to carry
the stored block in its own bytes, which raises the `ALLOC_MULTIPLE × input_length` term
along with it. So the remaining caps stay at their defaults.

**One shape exceeds the bound and is accepted rather than fixed, and this is the record of
why.** A `CONTINUE` chain is assembled from records, so its value is a concatenation that
exists nowhere contiguously in the input. `<Reference>` lets a small pool of records *compose*
a large number of distinct chains: `c` components with `k` alternatives each compose `k^c`
images, every one of them assembling a genuinely different value, so the amplification is
`images / k` and is maximized at the smallest `k`. Measured, at `Reader::seekable`
construction, with every count inside its cap:

| components | alternatives | images | input | allocated | bound | × input |
| --- | --- | --- | --- | --- | --- | --- |
| 2 | 16 | 256 | 8 232 036 | 509 402 249 | 271 813 760 | 61.9 |
| 4 | 4 | 256 | 4 245 920 | 1 035 238 533 | 144 258 048 | 243.8 |
| 8 | 2 | 256 | 2 172 332 | 1 054 515 007 | 77 903 232 | **485.4** |

The ceiling is `Images per source × Assembled keyword value × ~3.8` of assembly overhead —
**about 1.05 GB, from an input of about 2.2 MB.**

**I5 is not violated.** That figure is pinned to a product of two caps: it is finite, it is
computable in advance, and a larger input does not grow it. What fails is the oracle's
*proportionality*, which is a strictly stronger property than the "unbounded allocation" I5
names, and one this design does not promise.

**No constant separates this from the defect class it resembles, which is why the bound is not
raised to admit it.** The repeated-assembly defects and this shape allocate the *same* amount —
`images × assembled value`. They differ only in what the pool costs: a defect's images all
assemble one shared value, so the pool is one value's worth of input, while this shape's pool
is `k` values' worth. The ratios therefore differ by exactly `k`, and at `k = 2` the whole
discrimination window is a factor of two — the last such defect measured 982.5× against this
shape's 485.4×, a ratio of 2.02. A bound placed above 485 admits repeated assembly at up to
970×. Raising `ALLOC_MULTIPLE` here would not be a raise; it would be switching the oracle off
for this class.

**Tightening the fuzz `Limits` does not help either, and the paragraph above is right that only
two caps move.** Lowering `Images per source` and `Assembled keyword value` shrinks the
legitimate shape and the defect together, and both fall under the bound's fixed 8 MiB term
before the legitimate one fits: at 32 images and a 32 KiB assembled value, a repeated-assembly
defect allocates 4 MB against a 9.65 MB bound and passes. The tightening that makes the oracle
satisfiable is the tightening that blinds it.

So the disposition is: `tests/header_alloc.rs` pins this shape against its **absolute**
cap-product ceiling rather than a ratio, and a fuzz run that reports it is triaged against this
paragraph as accepted, not fixed. Re-fixing it is the failure mode this record exists to
prevent — the shape is arithmetic, and finding instances of it one at a time produced correct
fixes and no convergence. The fix that would end it is not a cap: it is
representing an assembled value as spans into a retained header buffer, materialized on demand,
so that a composed chain costs ranges rather than text. That is a public API change —
`Keyword::value()` cannot lend a `&str` it does not hold contiguously — and it is deferred, not
rejected. Revisit it if this crate is ever fed files from a source that did not produce them.

**Corpus discipline.** Seeds come from the synthetic fixtures, never from real frames — a
fuzz corpus is the easiest place for observatory coordinates to get committed by accident.
Every crash the fuzzer finds is committed as a regression seed once fixed. Fuzzing is the
enforcement mechanism for invariant I5 and the second enforcement path for I4.

### Dependencies

Verified current on 2026-08-18. The FITS reader is hand-rolled and has **no** runtime
dependency; every crate below serves XISF.

**Every runtime dependency is pure Rust, and that is a requirement rather than an
observation.** The reason is the cross-compilation story: a graph with no C in it builds for
any target the Rust toolchain supports with no host toolchain, no `cc`, no build script that
shells out, and no per-target packaging. That is what makes the `i686` and `wasm32` target
lanes cheap enough to run on every push, and those lanes are in turn what keeps
§ Normalization's target-scoping paragraph honest — the bit-exactness guarantee is stated
for `x86_64-*`, `aarch64-*`, SSE2 `i686-*` and `wasm32-*`, and a guarantee scoped to
targets nobody builds for is a guarantee nobody checks.

Two rows below are decided by this rule and would otherwise read differently: `flate2`'s
`rust_backend` over its C backends, and `ruzstd` over the far faster `zstd` bindings. Both
cost throughput — § Streaming's measured profile puts roughly 83% of a compressed decode
inside those two crates — and the trade is deliberate. A consumer that needs the C codecs'
speed more than it needs the target list should say so, and that is a change to this
section, not a local substitution in `Cargo.toml`.

| Crate | Licence | Role | Feature |
| --- | --- | --- | --- |
| `quick-xml` | MIT | XISF XML header, pull-parsed. Chosen partly for what it does *not* do: it skips DTDs rather than processing them, and resolves only the five predefined entities, non-recursively | `xisf` |
| `lz4_flex` | MIT | LZ4 block decompression. `default-features = false` with `std`, `safe-decode`, `checked-decode`. `std` must be listed explicitly — it is what pulls `alloc`, without which the allocating decompress entry points do not exist. Dropping the default set sheds the `frame` feature and its hash dependency, which serve LZ4's framed format; XISF stores bare blocks | `xisf` |
| `flate2` | MIT/Apache-2.0 | zlib. Its default `rust_backend` (`miniz_oxide`) is required, not incidental — the alternative backends are C, which the pure-Rust rule above rules out | `xisf` |
| `ruzstd` | MIT | `zstd` decompression, decode-only and pure Rust. `default-features = false` plus `std`: its default set enables `hash`, which pulls `twox-hash` — the very dependency the `lz4_flex` line sheds. Its `dict_builder` feature is also left off; that module is the one place in the whole graph that does floating-point arithmetic. It sets this crate's MSRV | `xisf` |
| `base64` | MIT/Apache-2.0 | `embedded` blocks in Base64. `default-features = false` and `std`, which keeps its hand-written SIMD path out of the build: this crate does not put SIMD `unsafe` on an attacker-reachable decode path to save microseconds. Confirm the exact feature name against the pinned manifest. The Base16 alternative needs no dependency at all | `xisf` |
| `sha1`, `sha2`, `sha3` | MIT/Apache-2.0 | block checksum verification | `checksum` |
| `thiserror` | MIT/Apache-2.0 | error derives | always |
| `fitsrs` | Apache-2.0/MIT | **dev-dependency only** — an independent FITS decoder to differential-test against | — |

**The ban on fused and fast-math helpers extends to any crate that uses them internally**,
which a grep of this crate alone does not discharge. The `deps-greps` lane therefore runs the
same grep across the **whole vendored dependency tree, transitively**. Three things about
that lane, all established by running the grep rather than predicting it.

First, **the lane must be scoped to the non-dev graph or it fails on its first run**: the
dev-dependency `fitsrs` pulls `wcs` and `mapproj`, spherical-projection crates where
`to_radians` and `powi` are everywhere, and `cargo vendor` materializes dev-dependencies.
**`cargo vendor` has no `--no-dev` flag** — checked against cargo 1.97.1, where the only
similar option is `--no-delete` — so the scoping runs the other way round: vendor the whole
graph, which is all cargo will do, then restrict the grep to the packages `cargo tree
--edges normal` reports. The job refuses to pass on an empty graph or an empty vendor
directory, because the obvious spelling passes *silently* when vendoring fails: grep finds
nothing in a directory that does not exist.

Second, **the non-dev graph needs one whitelisted package, and the entry is `typenum`.**
The RustCrypto generation this crate pins (`sha1`/`sha2` 0.11, `sha3` 0.12) reaches it
through `digest` → `crypto-common` → `hybrid-array`, and `typenum` defines a `powi` method
for *type-level integer* exponentiation on `Z0`/`PInt`/`NInt`. It contains no `f32` or `f64`
at any source line and computes nothing at runtime, so it is not on a decode path — which is
the review this section's own rule demands before a hit is dismissed, recorded here rather
than in a commit message. The whitelist is one entry, and the lane **fails if a whitelisted
package leaves the graph**, so the exception cannot rot into a silent hole.

Third, the two crates that *look* like candidates and cannot fire: `miniz_oxide` contains no
`f32` or `f64` at any source line, and `ruzstd`'s `dict_builder` uses `powf`/`ln`/`floor`,
which are not on the banned list and sit behind a feature that is off.

One further detail, learned the same way: the grep over **this crate's own** source strips
line comments first. The argument for why these constructs are banned is written *in* the
module that must not use them, so a grep over raw source flags its own rationale, and the
only way to keep the lane green would be to delete the explanation.

Versions are pinned in `Cargo.toml`, not here. No C dependency anywhere — that is
deliberate; a cfitsio binding was rejected partly because it lists Windows MSVC as
unsupported.

**Crate layout: one crate, per-format cargo features** (`fits`, `xisf`, both on by
default; `checksum` on by default). Not `astroframe-fits` + `astroframe-xisf` behind a
facade. The decisive argument is invariant I1 itself: the normalization primitive
must be *the same code* for both formats. A split puts it in a third crate that the two
format crates depend on by version range, which admits a build where the two are compiled
against different revisions of it and silently disagree — exactly the failure this library
exists to prevent.

**MSRV 1.88**, edition 2024 — **derived from this crate's own dependency set and its own
syntax, not inherited from any consumer**:

| Input | Declared or required `rust-version` |
| --- | --- |
| **let-chains under edition 2024** | **1.88** |
| `ruzstd` 0.9 | 1.87 |
| `sha1` 0.11, `sha2` 0.11, `sha3` 0.12 | 1.85 |
| edition 2024 itself | 1.85 |
| `lz4_flex` 0.14 | 1.81 |
| `quick-xml` 0.41 | 1.79 |
| `base64` 0.23, `thiserror` 2.0 | 1.71 |
| `flate2` 1.1 | 1.67 |

The maximum is **1.88**, set by the crate's own use of let-chains — `if let … && let …` and
`if let … && <bool>`, which edition 2024 stabilizes only from 1.88. The table lists the
language alongside the dependencies because both are inputs to the same maximum, which the
`ruzstd`-only reading of it missed: `ruzstd` 0.9's 1.87 is the highest *dependency* floor and
was mistaken for the whole answer.

That figure is **reported, not targeted**: this crate has no minimum-version obligation to
anyone, so the rule is to write the clearer code and record whatever floor results — never to
contort a call site, or reject a better library, for a lower number. Seven sites would have
had to be unrolled into nested `if`s, several of them then tripping `clippy::collapsible_if`,
to hold 1.87 for nobody's benefit.

**Verify the MSRV by invoking the toolchain's own `cargo` *and* `rustc` by path.**
`rustup run 1.88 cargo check` is not enough: where another Rust sits earlier on `PATH`,
rustup's shim loses and the check silently runs on the wrong compiler. That failure mode is a
*false pass*, and it is how the 1.87 figure survived a local check before CI rejected it.

### Operations

There is no migration story and no deployment: this is a library, starting at `0.1`. Its
build-time configuration surface is its cargo features, which means the features *are* the
untested configuration unless CI builds their powerset; its runtime one is `Limits`
(§ The caps), covered by the *Every cap has a test that trips it* criterion and by the fuzz
targets, which run tightened.

**It is published to crates.io**, and the manifest is what releases it. The release lane
reads `version` from `Cargo.toml` and cuts `v$version` if that tag does not exist, so bumping
the manifest **is** the release action. This repository computes no version from commit
messages, and the reason is that whether a change moves a decoded bit is a judgement about
arithmetic rather than a property of a `fix:` prefix — the decoded bits are part of the public
API, so a release that changes one ULP of output for an input that decoded before is breaking,
which at `0.x` means `0.1 → 0.2`.

One skew that topology makes silent is worth naming: a tag cut against a stale manifest ships a
crate whose own version disagrees with the ref that selected it, and nothing reports it. The
release lane publishes and then creates `v$version` *from* the manifest, so the two agree by
construction, and a tag exists only for a version the registry already carries. A tag pushed by
hand is checked against the manifest on its own lane, and never publishes.

The lanes below are the whole of CI. Two of them are unusual enough to name: no ordinary lint
stage carries grep-shaped checks, and few libraries this size fuzz.

| Lane | Runs | Contents |
| --- | --- | --- |
| `build-test` | every push | `cargo fmt --check`, `cargo clippy -D warnings`, rustdoc with `-D warnings`, and `cargo test` — the exhaustive normalization tests, the full adversarial suite, the allocation half of the peak-memory criterion, and the committed fuzz corpus replayed on stable. Then `cargo package`, which catches an `exclude` that drops a file the build needs |
| `build` | every push | The feature powerset, and the examples compiled with `-D warnings` |
| `msrv` | every push | `cargo check` on the floor the manifest declares, read *from* the manifest so the two cannot disagree |
| `targets` | every push | The exhaustive numeric tests on **32-bit** (`i686`, exercising the `usize` narrowing) and on `wasm32` (the NaN carve-out). Without it the bit-exactness guarantee is only ever proved on whichever architecture CI happens to run |
| `greps` | every push | The grep-shaped criteria over tracked files — banned constructs, fixture coordinates, machine-local paths, derived names, no committed frame, no redistributed specification, and a `use` of any format module inside `src/normalize.rs`, without which invariant I1's "mechanical enforcement" is enforcement by review |
| `deps-greps` | every push | The banned-construct grep across the whole non-dev dependency tree, vendored and scoped to the packages `cargo tree --edges normal` reports |
| `deny` | every push | `cargo deny check licenses` against an allowlist of permissive licences, plus an assertion that the crate ships both `LICENSE-MIT` and `LICENSE-APACHE` and that `Cargo.toml` declares `license = "MIT OR Apache-2.0"` |
| `fuzz` | scheduled | The exploratory run, crash artifacts published |
| `toolchain drift` | scheduled | Clippy on current stable, gating nothing, so a new lint arrives as a note rather than as a blocked merge; and `cargo deny check advisories`, so a fresh advisory arrives without turning an unrelated push red |
| `release` | pushes to the default branch touching `Cargo.toml` or `Cargo.lock` | Reads `version` from the manifest, checks the lockfile records the same version for this crate, and creates `v$version` if it does not already exist |
| `tag matches manifest` | tags matching `v*` | Asserts a tag pushed by hand matches the manifest's version. It publishes nothing: publishing happens in `release`, before the tag exists |

Superseded runs are cancelled, so two pushes a minute apart do not run two full pipelines side
by side. The release and publish lanes opt out: creating a tag is not idempotent between the
existence check and the create, and a cancelled gate reads as an unrun one.

**No lane depends on the local corpus.** The corpus-backed checks are developer-invoked
only (§ Local corpus validation); CI runs entirely on fixtures constructed in the test
source.

One combination needs a stated answer: with **both** `fits` and `xisf` disabled the crate
still compiles and still exposes `Header`, `Samples` and the normalization primitive, but
every constructor returns `Unsupported` — the format-independent layer is useful on its own
and there is no reason to make the combination a build error. `checksum` without `xisf` is
inert.

The `deny` lane is not ceremony. This crate's entire premise is that it is written from
published specifications alone and so can be permissively licensed; a transitive dependency
pulling in a copyleft licence would silently destroy that, and no grep for derived names
would catch it.

Two constraints shape that table. An open-ended fuzz run is worth minutes nobody is waiting
on, so it is scheduled rather than per-push — while the *regression* half of fuzzing, the
cheap half that catches reintroduced bugs, runs on every push. And `cargo-fuzz` needs a
**nightly toolchain** for its sanitizer flags while this crate's floor is stable 1.88, so
the `fuzz` lane is pinned to nightly, is allowed to lag, and must never become a gate on
the stable build or a reason to raise the MSRV. Keeping the replay on stable is what makes
that separation affordable: the seeds are fed through the same entry points by a plain
loop, no nightly required.

### Deliberate divergences from prior art

**This is the single home for these.** The prior XISF decoder studied here is prior art to
mine, not behavior to inherit — its adversarial corpus is ported wholesale while its
*decisions* are re-taken here. Each row below is a choice, and every one of them changes an
observable outcome. Any behavioral difference not listed is still deliberate until this
table or § Alternatives says otherwise; the list is the known set, not a closed one.

| Prior art does | `astroframe` does | Why |
| --- | --- | --- |
| Divides per sample (`float32(u16) / 65535`) | Multiplies by a rounded `f32` reciprocal | The load-bearing convention. § Normalization |
| Defaults float `bounds` to `0:1` when absent | `Unavailable(InvalidDeclared)`; native samples still decode | § Normalization. Defaulting a mandatory attribute produces plausible-looking wrong pixels for an invalid file |
| Folds NaN to zero | Preserves NaN | § Normalization. Turning "no data here" into "black data here" propagates into statistics as a real measurement |
| Parses `bounds` only for float formats | Parses and validates it for every format | A malformed `bounds` on a `UInt16` image is ignored there and reported here |
| Clamps in `f64`, then narrows | Clamps in `f32`, after the multiply | The pinned three-step form leaves no `f64` after step 2 |
| **Validates** channel count against the colour space's nominal count — `Gray` must have exactly 1, `RGB` exactly 3 — rejecting `gray with two channels`, `rgb with four channels` and `colorspace absent with three channels` | Decodes all three | § XISF decisions — channels beyond the nominal count are alpha channels (§8.5.1). Note what the divergence is **not**: that decoder defaults an absent `colorSpace` to `Gray` exactly as this one does, and says so in its own comment. The difference is the validation that follows the default, and it is the mechanism behind three of the eleven rebuilt fixtures |
| Rejects a `shuffle item size mismatch` | Decodes it | § XISF decisions — the specification never ties `item-size` to the sample width, so that decoder is over-strict |
| Refuses the `attached:` location spelling | Accepts `attached:` alongside `attachment:` | § XISF decisions — the specification's own examples use it |
| Reads `Property` only from inside `<Image>` | Reads all three scopes and tags each | A conforming file carrying `Observation:Time:Start` at root or `Metadata` scope yields no timestamp at all there |
| Implements `base64` embedded encoding only, rejecting `hex` | Supports both | § XISF decisions — §10.3 admits both. The Base16 half is net-new work rather than a port, so its test is the only evidence it will get |
| Ignores `checksum` and `subblocks` entirely — a file declaring a non-matching digest decodes clean there | Verifies every checksum it reads; honours `subblocks` | § XISF decisions. That a non-matching digest decodes clean in mature prior art is the strongest independent argument for the mandatory-verification stance |
| Validates in the order geometry → colorSpace → sampleFormat → **caps** → byteOrder → pixelStorage → **bounds** → location → compression → offset | Moves the caps and `bounds` out of the header phase; otherwise the same order | § Errors → Validation order. This is what forces eight adversarial fixtures to be rebuilt rather than byte-ported — the five `bounds` cases and the three cap cases; the other three rebuilds are the channel-count divergence's, per the row above |
| Refuses a unit carrying no `<Image>` element | Walks zero images: construction succeeds and `next_image()` returns `Ok(false)` | § Errors → Where a decline surfaces. A header that parses and declares no images is readable, and the FITS analogue — a primary with `NAXIS = 0` and no image extension — is settled the same way |
| Has no cap on total XML element count | Caps it | § The caps. A review of that decoder flagged the absence |
| Classifies a bad `pixelStorage`/`byteOrder`, a header-length trip and an image-count trip as corrupt-data; byte-cap and geometry-overflow trips as bad-geometry | Caps are `LimitExceeded`, malformed enumerations are `Malformed`, declined-but-valid features are `Unsupported` | § Errors. Derive each ported case's class from those rules, never from the case name |
| Compares against its golden model with an absolute epsilon of `1e-6` | Compares with `f32::to_bits()` | § Testing mechanics — nothing upstream will catch a one-ULP regression on this crate's behalf |

## Alternatives

Each row is an approach that was seriously considered and rejected. They are recorded so
they are not re-litigated, and because several of them are the *obvious* answer — a future
reader who does not find the argument here will reach for them again.

| Alternative | Why it was rejected |
| --- | --- |
| **Normalize with `x as f32 / 65535.0`** | Correctly-rounded division is one rounding; the contract needs two. Differs on 0.78% of 16-bit levels and **49% of 8-bit levels**. A difference of exactly this class has already shifted 4 of 17 frame medians on real data. |
| **Offer both normalization forms behind a mode flag** | Makes the wrong bits reachable by configuration, turns the cross-format bit-identity guarantee into a conditional one, and doubles the test matrix. There is no sense in which division is "more correct" for a value whose full scale is a convention. A caller who wants different arithmetic takes native samples and does it themselves — which the layering already provides, at zero API cost. |
| **A `Parity` / "application-validated" marker on the decoded frame** | Declined on **ownership**, not availability. A marker is a *claim about a particular application's behavior* — that its extra normalization pass runs only for IEEE-float images, say — and this crate is in no position to make one: it reads files, not applications. Such a claim also ages badly, since the marker would silently become wrong the day that application changed the pass, in a crate with no way to know. What this crate can report it does: the header carries every fact the predicate is computed from (§ The API), and the predicate itself is policy and belongs to whichever consumer needs it. |
| **Reject FITS frames with `ROWORDER = 'BOTTOM-UP'`** | Hostile to every consumer that can handle a flip, and the flip policy differs per consumer. Reporting is strictly more informative and cannot be silently wrong the way a rejection paired with stale documentation can be — that class of bug is a *doc/code mismatch*, not a consequence of reporting. |
| **Flip bottom-up frames so output orientation is uniform** | Applies a geometric transform nothing in the file asks for, moving every star coordinate — and a consumer that wanted stored order has no way back. XISF §11.5.2 independently forbids applying `orientation` for processing-oriented loads, so flipping would also make the two formats behave inconsistently. |
| **Reject frames carrying `PEDESTAL`** | A pedestal belongs to a *measurement* — it is subtracted from a reported statistic, not from pixels — which makes it a measurement-stage concern with a single natural owner. Rejecting here means the keyword is owned twice and the two owners disagree. |
| **Subtract `PEDESTAL` / XISF `offset` during decode** | Same defect from the other side, plus it destroys information: a consumer cannot recover the original samples. |
| **Normalized `f32` as the only pixel output** | Forces one consumer's convention on every other consumer, loses ADU exactness for `UInt32`/`Float64`, and would have made FITS float frames flatly undecodable rather than decodable-without-normalization. |
| **Reject FITS float `BITPIX` outright** | The simple answer, and needlessly lossy. FITS defines no representable range for floats, so only the *normalized* output is undefined — the samples themselves are perfectly decodable. Refusing just the normalized output, and letting a caller supply bounds, costs one method. |
| **Split into `astroframe-fits` and `astroframe-xisf` behind a facade** | The shared normalization primitive would land in a third crate reachable by version range, admitting builds where the two formats normalize differently. That is the precise failure the library exists to prevent. Cargo features give the same dependency trimming with no such seam. |
| **A public `FrameDecoder` trait, with the format decoders exposed behind it** | Two shapes are available — one `Frame` type for both formats, or format-specific types behind a shared trait. The type answer is one shared pair, and the trait answer is no for v1. With two formats and no promised extension point the trait abstracts over nothing a caller touches. The chunk API also does not fit: `next_chunk()` hands back a chunk borrowing the reader's scratch buffer — a lending cursor, which is not object-safe — so the trait could not be used as `dyn FrameDecoder` anyway, which is the only thing a public trait would have bought. Adding it when a third format arrives breaks no existing shape. |
| **Per-format error types behind a shared trait** | Forces callers to be generic over error types for no benefit, and boxing them loses the skip-versus-abort distinction that is the only classification consumers actually need. |
| **Use `fitsio` (cfitsio FFI) or another FITS crate** | `fitsio` reintroduces a C dependency and lists Windows MSVC as unsupported. `fitrs` and `rustronomy-fits` are GPL-3.0 and unmaintained. `fitsrs` is the strongest third-party option and is used — as a **dev-dependency**, to differential-test against. The decisive argument for hand-rolling is not size but that `BSCALE`/`BZERO` application is where bit-exactness is first lost, and it must be under this crate's own control: a crate applying that scaling in `f64` reintroduces the exact 1-ULP defect § Normalization exists to prevent. |
| **Return an `Iterator` of chunks** | A chunk borrows the reader's scratch buffer, so `Iterator` cannot express it without allocating per chunk. The `next_chunk()`-in-a-`while let` shape is the established Rust answer for lending iteration. |
| **Require `Read + Seek` everywhere** | Locks out pipes, sockets and decompressed streams — precisely the callers streaming exists to serve. FITS never needs it, and monolithic XISF needs it only for out-of-order blocks. |
| **License under MIT alone** | MIT alone is permissive enough and would satisfy every constraint this crate is under. It buys nothing the dual licence does not, and gives up Apache-2.0's explicit patent grant for consumers that want it, against an ecosystem that expects the pair. § Licensing boundary. |
| **Verify block checksums only on request** | XISF §10.5 makes verification mandatory when the attribute is present, so skipping it silently is a conformance gap. The `checksum` cargo feature governs only whether the verification code and its three hash dependencies are *compiled in*, and building without it does not weaken the guarantee: a block that declares a checksum is then refused as `Unsupported`. One consequence is worth stating because it is not obvious: §9.5 and §10.5 require every block of a **digitally signed** unit that is not serialized directly in the header to carry a checksum, so an `xisf`-without-`checksum` build cannot decode a signed unit whose pixels are attached — and with the feature on, every signed unit is forced to `WholeImage` granularity by the checksum floor. |
| **Decline `zstd`-compressed blocks** | The argument for declining is that `zstd` appears nowhere in XISF 1.0, so implementing it would exceed the specification this crate is written from — but that conflates two disciplines. The rule this design keeps is that every format rule is taken from a published specification and every behaviour is verified against real files; decoding a documented, freely-specified compression format sits squarely inside that, and "never exceed the spec" is not the rule. Meanwhile the cost of declining is concrete: PixInsight writes `zstd` blocks, and the local corpus contains 240 such files. Refusing them would make the crate fail on real output to preserve a boundary that was not at stake. |
| **Eagerly enumerate all images/HDUs at header time** | Free for XISF (they are all in the XML header) but for FITS it means reading past every data unit, which would make tier-1 header decode cost a full pass over the file. Forward-only `next_image()` keeps tier 1 cheap for both. |
| **Select the first image at construction** | Every multi-extension FITS file and every tile-compressed file has a primary with `NAXIS = 0`, so on those files there is no first image to select and the constructor has nothing to sit on. Selecting nothing at construction and advancing with `next_image()` holds for every file layout instead (§ The API). |
| **Have construction sit on the declined image** | The other way to give the constructor something to select: on a file whose primary is `NAXIS = 0`, advance to the first position even when it is one this version declines. Reaching a `BINTABLE` past that primary means skipping a data unit, which contradicts the criterion that construction touches only a header region. |

## Acceptance Criteria

Every item is observable and checkable, and a later review grades the implementation
against this list. Criteria are referenced **by name**, here and everywhere else in this
document — numbers shift whenever one is inserted, and a stale number silently maps an
invariant onto a criterion that does not check it.

**Two tiers of evidence, and the boundary between them is a hard rule.**

| Tier | What it is | Where it runs |
| --- | --- | --- |
| **Committed fixtures** | Small, synthetic or scrubbed files, every byte constructed in the test source | Every push, in CI. This is the whole of the automated suite |
| **Local corpus validation** | 84 GB of real PixInsight, AstroPixelProcessor, N.I.N.A., ASIAIR and CFITSIO output, held on one machine | **Developer-invoked only, never in CI.** § Local corpus validation |

The corpus is **never committed and never automated**: it is 84 GB, and it carries
observatory coordinates at full precision. No CI lane may depend on a file held on one
machine. Everything in the numbered criteria below is fixture-borne unless it names the
corpus tier explicitly.

**Two facts about where it came from, because each one cuts a different way.**

It is **not one instrument's data**. The frames span several camera, mount and
capture-software combinations, which is what makes the corpus evidence rather than an
anecdote: a decoder that agrees with a single writer on a single instrument has learned that
writer's habits, not the format. The differential in § Local corpus validation is worth what
it is because the variants disagree with each other about how to write the same frame.

And precisely because it is not one instrument's data, **the never-commit rule is not the
maintainer's to relax.** These frames record sites and observing times at full precision. That
makes scrubbing an obligation to whoever the data belongs to — who is not in this repository
and cannot review what it publishes — rather than a preference about one's own privacy — so the rule holds even where a maintainer would happily publish their own
coordinates, and it holds for anything derived from a frame: a fuzz seed, a fixture, an issue
report, a pasted header.

### Numeric correctness — the ones that matter most

1. **Exhaustive `UInt16` normalization.** All 65 536 levels, decoded at the default
   representable range, compared by `f32::to_bits()` against the pinned form
   `level as f32 * (1.0f32 / 65535.0f32)`. Zero mismatches.
2. **Exhaustive `UInt8` normalization.** All 256 levels, same method, against
   `1.0f32 / 255.0f32`.
3. **The divergence is pinned, not merely avoided.** A test asserts that the multiply form
   and the division form differ on exactly **512** of 65 536 16-bit levels and exactly
   **126** of 256 8-bit levels, with the first differing 16-bit levels being 257, 261, 265,
   269, 273, 277. If someone "simplifies" the primitive to a division, this test says so in
   plain numbers.
4. **Endpoints are exact at every integer width.** Level 0 decodes to `+0.0f32` (sign bit
   clear, checked via `to_bits`) and full scale decodes to exactly `1.0f32`, for `UInt8`,
   `UInt16`, `UInt32` **and `UInt64`**. A companion test records the other half of that
   story: for `UInt32` and `UInt64`, distinct stored levels are shown to collide in the
   normalized output, so the lossiness is pinned as intended behavior rather than
   discovered later as a bug. It must sample **high** levels — collisions do not occur at
   the bottom of the range, where `f32` spacing is far finer than the level spacing, so a
   test picking levels 0 and 1 would fail a correct implementation.
5. **Non-finite handling is total.** A float source carrying `+Inf`, `-Inf` and `NaN`
   decodes to exactly `1.0`, `+0.0` and `NaN` respectively, compared by `to_bits()` (by
   `is_nan()` for the NaN case on `wasm32`). A sample below `lo` decodes to `+0.0` with the
   sign bit clear, never `-0.0`.
6. **Range validity is one rule about `k`, enforced at both entry points.** Rejected
   whether they arrive from a file-declared `bounds` or from `Reader::with_bounds` — the
   same *rule*, deliberately not the same error class: a file-declared bad `bounds` is
   `Malformed` and a bad `with_bounds` is `InvalidRequest`. The cases: `lo == hi`;
   `lo > hi`; non-finite endpoints; and — the cases endpoint checks miss — finite ordered
   endpoints whose width underflows to zero or a subnormal in `f32` (`0`, `1e-46`), or
   overflows it (`-1e308`, `1e308`). The test asserts on `k`, not on the endpoints.
7. **Cross-format bit-identity.** A synthetic FITS frame (`BITPIX=16`, `BZERO=32768`,
   `BSCALE=1`) and a synthetic XISF frame (`UInt16`) carrying the same sample values decode
   to `to_bits()`-identical buffers. Run for uncompressed XISF, for `zlib+sh`, and for
   `lz4+sh`, so compression and shuffling are proven not to perturb output.
8. **Streaming equals whole-buffer, bit-for-bit.** For every fixture, the buffer from
   `read_image_into` and the buffer assembled from `chunks()` are `to_bits()`-equal. Run
   for both formats and for each streaming granularity.
9. **Differential FITS check.** The hand-rolled reader's native samples — the raw stored
   values, before any `BSCALE`/`BZERO` — match `fitsrs`'s for every pixel. Comparing
   *native* samples is the point, not incidental: any FITS decoder applying `BSCALE`/`BZERO`
   in `f64` reintroduces the exact 1-ULP defect this design exists to prevent, so the
   comparison is taken upstream of scaling, where the two implementations must agree
   bit-for-bit regardless of what either does afterwards.
   `fitsrs` is pinned at 0.4.1 and its scaling behaviour was checked rather than assumed —
   and the check came back stronger than expected: it applies `BSCALE`/`BZERO` on **no** path
   at all. The only two source lines naming those keywords sit inside a `#[cfg(test)]` module
   on the tile-compressed path, where the *caller* does the arithmetic to build a preview;
   the library itself never scales, and the `Image` xtension does not even model the two
   keywords as fields. Its sample iterator does exactly one thing per sample, a big-endian
   read into the native type. So the comparison needs no "give me unscaled values" entry
   point, and the property this criterion rests on is unconditional rather than
   path-dependent. Runs over the committed fixtures in CI, and over the corpus by hand
   (§ Local corpus validation), where it catches header-parsing and endianness errors a
   synthetic fixture cannot.
10. **No banned construct on the decode path.** A CI check greps the crate for `mul_add`,
    `algebraic_`, `powi`, `to_degrees`, `to_radians`, and `fmuladd`, and fails on a hit
    outside a test asserting their absence.

### Behavioral correctness

11. **Report-don't-interpret is observable.** A `ROWORDER = 'BOTTOM-UP'` FITS frame and an
    `orientation="180"` XISF frame each decode successfully, deliver samples in stored order
    (asserted against a known pixel pattern), and report the metadata verbatim.
12. **`PEDESTAL` and XISF `offset` change no pixel.** Two otherwise-identical fixtures
    differing only in the presence of the keyword decode to identical buffers, and the
    keyword is retrievable.
13. **Keyword lookup does not case-fold.** `get("SITELAT")` and `get("sitelat")` do not both
    resolve; names are stored and matched exactly as they appear in the file. Duplicate
    keywords (`HISTORY`, `COMMENT`) are all retrievable, in document order.
14. **Header-only decode reads no pixel bytes.** With a source that records its reads,
    constructing a `Reader` touches only the header region. This is the check that tier 1 is
    real rather than nominal.
15. **Header-only decode works on a truncated prefix.** A source containing only the header
    bytes and then ending yields a complete `Header` with no error, for both formats. A
    consumer that fetches a size-capped prefix of a remote file depends on this, and it is
    easy to break by validating a declared block offset against a file length the source
    does not have. The adversarial `attachment out of bounds` case asserts that a known-length
    source rejects a bad offset when the block is reached; this criterion asserts that the
    check does not fire early.
16. **FITS float frames decode natively and refuse normalized output.** `read_samples_into`
    succeeds; `read_image_into` returns `Unsupported`; after `with_bounds(lo, hi)` it
    succeeds.
17. **Declining is catchable, and distinguishable from failing.** A `Complex32` XISF frame
    yields `Unsupported`; a truncated file yields `Malformed`; a source whose reads fail
    yields `Io` with `is_io()` true. A tile-compressed FITS file yields `Unsupported` rather
    than being misread as a table.
18. **Reported metadata is reachable.** For an XISF frame declaring `orientation`, `offset`,
    `bounds`, `colorSpace`, `pixelStorage`, `id`, `uuid` and `imageType`, each is retrievable
    from `Header` by its own accessor — not via keyword or property lookup, which do not
    reach XML attributes. This is what makes report-don't-interpret checkable rather than
    aspirational.
19. **A keyword reads the same from either container.** The same logical keyword written as a
    FITS card and as an XISF `FITSKeyword` (whose `value` attribute carries the FITS quoting)
    yields byte-identical strings from `get`. `HISTORY` and `COMMENT` text lands in the same
    field for both formats.
20. **Metadata that has no FITS equivalent survives, in all its shapes.** An XISF frame
    carrying an attribute-valued `Property` and one carrying a **character-data** `String`
    property both expose it, as an `(id, type, value)` tuple with `format` and `comment` when
    present — a `String` property has no `value` attribute by specification, so testing only
    the attribute-borne shape would pass while dropping every `Observation:Object:Name` in
    existence. A block-valued property is reported with value `Unavailable` rather than
    dropped, carrying its type. A frame carrying duplicate `HISTORY` keywords exposes all of
    them, in document order. The reported `type` is the classified enum, graded on all three
    of its cases: a primary specification name resolves to its variant, an **alternate**
    spelling (`Byte`, `Vector`) resolves to the same variant as its primary rather than to
    the catch-all, and an unrecognized name is preserved verbatim in `Other` — the shape
    `ROWORDER` already uses for a spelling it does not know.
21. **`CONTINUE` and `HIERARCH` fold.** A `CONTINUE` chain yields the assembled value with no
    trailing `&`, under the name on its first card; a `HIERARCH` card yields its full
    multi-word name, and two such cards do not collide. Both §4.2.1.2 edge cases are graded
    alongside them: a value ending in `&` with no conforming `CONTINUE` record after it keeps
    the `&`, and an orphaned `CONTINUE` record is reported as commentary text. Those two are
    where an unconditional fold — the obvious implementation, and a common one — silently
    corrupts a value.
22. **Non-conforming header text is handled by the stated rule.** A FITS frame with non-ASCII
    bytes in a `COMMENT` card parses and its geometry survives; the same bytes in a value
    field are a hard error.
23. **Every decline surfaces where § Errors says it does.** Each row of the decline table
    yields all three of its stated facts: its class, its surfacing point — construction,
    `next_image()`, or a declined position with `decline_reason()` set and any pixel call
    failing — and the geometry its last column states, since a declined position reporting
    `None` where the table says full geometry, or the reverse, is a defect the class and the
    surfacing point both miss. A **Neither** row is graded as a walk that ends normally with
    no error, and a row surfacing at `next_image()` is graded as `Err` rather than
    `Ok(false)`. That table is the list; this criterion grades against it rather than
    re-enumerating it.
24. **Multi-image and source-mode behaviour.** A file using the deduplicated
    `Reference`-to-`Image` spelling reports one occurrence per `Reference`, not one per
    `Image` element. On a sequential source the second such occurrence is `Unsupported` (its
    block lies behind the cursor) while the same file decodes fully through `Reader::open`.
    Reading the same fixture via `open`, `sequential` and `seekable` yields
    `to_bits()`-identical buffers. `with_bounds` and `select_channel` reset across
    `next_image()`, and a per-image attribute fault declines that image without failing the
    source.
25. **`select_channel` decodes the same bits as slicing a full decode.** For a multi-channel
    fixture, `select_channel(k)` then `read_image_into` is `to_bits()`-equal to
    `Image::channel(k)` of an unnarrowed decode — run for both `Planar` and `Normal` storage,
    since the interleaved path is a transposition and a transposition is where this silently
    corrupts. It also pins the two numbering schemes: `Image::channel` indexes the image,
    `channel_index()` and chunks index the file. The corpus has no `Normal` storage and one
    RGB master, so this is fixture-borne and is the only thing standing between the
    interleaved path and silent corruption.
26. **`granularity()` reports the right value.** One fixture per row of the streaming table,
    asserting the *reported* `Granularity`, not merely that decode succeeds. The combinations
    a first-match implementation gets wrong are the point: `subblocks` **+** shuffling must
    report `WholeImage`; `subblocks` **+** checksum must report `WholeImage` too; `lz4` **+**
    `subblocks` with neither must report `Block`; `zlib` **+** `subblocks` must report `Rows`,
    since `subblocks` only blocks a promotion and never lowers a `Rows` floor. An `embedded`
    source reports `WholeImage`.
27. **Baseline XISF decoder conformance.** Each of the abilities XISF §7.2 requires of a
    baseline decoder has a test demonstrating it: monolithic files; multiple `Image` elements
    from one file; every standard compression codec; `embedded` and `attachment` pixel
    locations (the partial one — no inline pixel data, per § Format support matrix); `Planar`
    and `Normal` storage; `UInt8`, `UInt16` and `Float32` sample formats; `Gray` and `RGB`
    colour spaces. This is what makes the format matrix's declines defensible rather than
    gaps — everything declined sits above baseline.

### Format decisions, each pinned by a test

28. **Every FITS decision in § FITS decisions has a test.** Individually small, collectively
    the largest untested surface on that side: `PCOUNT`/`GCOUNT` data-unit skipping past a
    heap-carrying `BINTABLE` (which the design itself calls a prerequisite for recognizing a
    tile-compressed file at all); big-endian sample decode; the `INHERIT` precedence rule,
    graded directly — an extension carrying its own `BSCALE` under `INHERIT = T` decodes with
    **its own** value, and an extension carrying `BSCALE` but no `BZERO` under `INHERIT = T`
    decodes with its own `BSCALE` beside the primary's `BZERO` — **and** its `INHERIT = F`
    exception; `NAXIS = 3` read as channels; the signed-byte convention
    (`BITPIX = 8`, `BZERO = -128`) refusing normalized output; `BLANK` reported and not
    substituted; `CHECKSUM`/`DATASUM` reported without verification; `ROWORDER` spelling
    normalization with `Other` preserving an unrecognized value verbatim.
29. **Every XISF decision in § XISF decisions has a test.** Big-endian block decode
    (`byteOrder="big"` — the document's own highest-risk pinned default, since a wrong guess
    corrupts every sample silently rather than erroring); the `attached:` alternate location
    spelling; `hex`/Base16 embedded encodings; whitespace stripped before Base64/Base16 decode
    and around plain-text scalars, but **preserved** inside a `String` property; character
    data assembled across CDATA and entity boundaries before either rule applies; a
    namespace-prefixed header; entity references resolved before a value is reported;
    `Reference` resolution, its forward-reference case and its pinned ordering; a `Reference`
    to a nonexistent `uid`; the three subblock length-sum checks; `item-size == 1` as a no-op
    and a trailing partial item copied through; `compression` read from the child `<Data>`
    element for an embedded block; a thumbnail's attached block skipped without allocation;
    an `attachment` position inside the header region refused.

### Hardening criteria

30. **Adversarial suite: the prior art's cases, ported one for one.** The XISF adversarial
    suite is not a list invented here — it is a prior XISF decoder's own adversarial table of
    35 named cases, the accumulated record of what malformed XISF actually looks like. Every
    one is ported, **keeping the case name** so the two suites can be diffed:

    `attachment out of bounds`, `bad signature`, `bounds equal`, `bounds nan`,
    `bounds negative infinity`, `bounds positive infinity`, `bounds reversed`,
    `colorspace absent with three channels`, `compressed usize disagrees with geometry`,
    `geometry integer too large`, `geometry non-numeric field`, `geometry non-positive field`,
    `geometry overflow`, `geometry too few fields`, `gray with two channels`,
    `header length overruns file`, `higher-dimensional geometry`, `lz4 decompression bomb`,
    `malformed xml`, `negative offset`, `no image element`, `output byte cap exceeded`,
    `rgb with four channels`, `sample block byte cap exceeded`, `shuffle item size mismatch`,
    `too short for preamble`, `truncated mid-block`, `uncompressed attachment too large`,
    `uncompressed block size mismatch`, `unsupported codec`, `unsupported color space`,
    `unsupported sample format`, `wrong root element`, `zero-size attachment`,
    `zlib decompression bomb`.

    **Not all of them can be ported as bytes, and the reason is this design's doing rather
    than the source suite's.** That decoder validates `bounds` and the caps *before*
    `location`; this design moves both out of the header phase (§ Errors → Validation order),
    and three colour-space cases it rejects are decoded here. Of the 35 fixtures, 23 carry no
    `location` attribute at all; five of those never reach an `<Image>` element, leaving **18
    whose intended fault is an image-attribute fault that decoder reaches before it looks for
    a location**. The split that results:

    | Group | Cases | Port |
    | --- | --- | --- |
    | Never reach an `<Image>` element | `bad signature`, `too short for preamble`, `wrong root element`, `malformed xml` | **Bytes port unchanged**; fail at construction |
    | Never reach an `<Image>` element, and not an error here | `no image element` | **Bytes port unchanged**, assertion flipped: construction succeeds and `next_image()` returns `Ok(false)` (§ Errors → Where a decline surfaces) |
    | Geometry and sample format, header-only | `geometry integer too large`, `geometry non-numeric field`, `geometry non-positive field`, `geometry too few fields`, `higher-dimensional geometry`, `unsupported sample format` | **Bytes port unchanged** — geometry is first in both validation orders. `geometry too few fields` splits: fewer than two fields is `Malformed`, exactly two is a valid 1-D image and `Unsupported` |
    | Colour space, header-only, still an error | `unsupported color space` | **Bytes port unchanged** |
    | Colour space, header-only, now a **decode success** | `colorspace absent with three channels`, `gray with two channels`, `rgb with four channels` | **New fixture bytes.** The originals have no `location` and no block, which is adequate for asserting an error and useless for asserting a decode. Keep the case name and the attribute combination — that is what is being diffed — and add a minimal valid attachment and expected pixel values |
    | `bounds`, header-only | `bounds equal`, `bounds reversed`, `bounds nan`, `bounds positive infinity`, `bounds negative infinity` | **New fixture bytes.** With no `location` the missing-location rule fires first here and the frame is `Malformed` at construction, so the intended assertion never runs. Rebuilt with a valid location and a real block, they fail at `read_image_into` — which is where `bounds` is raised. (They are weak in the original too: a missing `location` also yields its corrupt-data class there, so they pass for the right reason only by ordering.) |
    | Caps, header-only | `output byte cap exceeded`, `sample block byte cap exceeded`, `geometry overflow` | **New fixture bytes**, for the same reason: the caps run in the pixel phase here. `geometry overflow` belongs in this group rather than the geometry one — its `geometry="100000:100000:1000"` is three positive integers and parses, and what that decoder catches is its total-samples ceiling (`maxSamples = 1 << 30`) under a class name that describes the arithmetic rather than the check. Rebuilt keeping the case name and the geometry, with a valid location and a real block, it asserts `LimitExceeded` at the pixel phase — the class § Errors derives for a cap trip, and determinate because the total-samples cap runs first there (§ Errors → Validation order) |
    | Carry a `location` | the remaining 12, including both decompression bombs | **Bytes port unchanged**, except `shuffle item size mismatch`, whose bytes port but whose assertion flips from an error class to a successful decode with expected pixel values. Carrying a `location` is what these have in common and what carries them past the location check this design runs last — not carrying block bytes: four have none behind the location, `negative offset`, `unsupported codec` and `attachment out of bounds` spelling one inline with no block appended at all, and `zero-size attachment` declaring a zero-length one |

    So **24 cases port as bytes and 11 need new fixture bytes**; **four assert a decode rather
    than an error** (the three colour-space cases and `shuffle item size mismatch`) and a
    fifth, `no image element`, asserts an empty walk — because this design deliberately
    accepts what that decoder rejects (§ Deliberate divergences from prior art). Authoring eleven fixtures
    is the right answer — the alternative is putting `bounds` back in the header phase,
    which would make an unnormalizable frame unreadable.

    **Every other case asserts both the expected error class and that no value is returned
    alongside the error**, with the class derived from § Errors rather than from the case
    name: that decoder's classes do not map onto this design's. Three cases check invariant
    I4 directly rather than incidentally — `compressed usize disagrees with geometry`,
    `uncompressed block size mismatch` and `sample block byte cap exceeded` — being the
    cases where a declared size is used as a cross-check instead of an allocation size.

    **The tests outside that table are ported too**, and what two of them contribute has to
    be said plainly, because their names claim more than their bodies do.
    `TestBombDoesNotAllocate` and `TestCapDoesNotAllocate` measure no allocation: the first
    asserts only a corrupt-data class on a zlib bomb, the second only a bad-geometry class on
    a geometry inside the sample ceiling but past the byte cap. What ports is the **case**,
    not the assertion — the two inputs are the contribution, and here they carry the real
    instrumentation that *A decompression bomb is refused without materializing* and *Peak
    decode memory meets the stated target* already mandate, which is where this design's
    allocation assertions live. An implementer porting the two by name will otherwise
    believe the assertion came with them, and end up with the invariant named in a test
    function that does not check it. `TestIOErrorClassified` is the one that does assert
    what it claims — an I/O classification on a reader that fails mid-read rather than at
    end of file — and ports as it stands. The rest are `TestImageCountCap`, the `FuzzDecode`
    target with its nine seeds, and `TestRealCorpus` — the only one touching real bytes,
    which cross-checks header-only geometry against single-image decode and the image
    *count* against all-image decode. It never compares pixels, so the cross-entry-point
    *pixel* check is this design's addition rather than an inheritance, and both live in the
    corpus tier.

    **Cases the ported suite has no reason to cover, added here:** FITS-side malformed input
    (bad `SIMPLE` card, no `END`, truncated header block, `NAXIS` inconsistent with the data
    unit length, non-ASCII in a value field); tile-compressed FITS declined; the XML-guard
    cases; an `<Image>` with **no `location` at all** — net-new, since every ported fixture
    that reaches that check supplies one; a `Reference` pointing at a nonexistent `uid`; a
    subblock list whose lengths do not sum to the block size; a checksummed block whose
    digest does not match; a **zstd window bomb**; and a block behind the cursor on a
    sequential source. Cases beyond these are welcome; dropping one the ported suite covers
    is a regression.
31. **Caller misuse is an error, not a panic.** A wrong-sized destination slice,
    `select_channel` beyond the channel count, and `with_bounds` after the pixel phase each
    return `InvalidRequest`. The last-wins pair is graded the other way, since the boundary
    is what an implementer gets wrong: a second `with_bounds` and a second `select_channel`
    *before* the pixel phase each succeed, and the frame decodes against the later value.
    A second `with_bounds` carrying an invalid range returns `InvalidRequest` and leaves the
    first range in force.
32. **Every cap has a test that trips it**, and each returns `LimitExceeded`: every row of
    the caps table. The stored-block cap is the one that closes I4's remaining hole, so it is
    the last one that should go untested. The FITS caps are tested over a non-seekable source
    with no known length, which is the case that has no file size to fall back on. **`Limits`
    is exercised in both directions**, since a parameter nothing moves is a constant: a
    fixture decoding cleanly under `Limits::default()` returns `LimitExceeded` under a
    lowered cap, and a fixture tripping a default cap decodes cleanly under a raised one —
    run for `decoded_output_bytes`, the cap the drizzle case moves. **The total-samples cap is
    graded under `select_channel` as well**, since counting the narrowed channel instead of
    the file's is the plausible implementation and no other case distinguishes them: a
    narrowed reader on a multi-channel frame whose *file* geometry carries it past the cap
    still returns `LimitExceeded`. On 32-bit, a cap raised past what `usize` addresses
    returns `LimitExceeded` at the narrowing rather than panicking; that one belongs to the
    `targets` lane's `i686` job.
33. **The XML guards, each with its own test**: an oversized declared header length rejected
    before the read; `DOCTYPE` rejected; a billion-laughs header rejected; a header nested past
    the depth cap rejected rather than overflowing the stack; a header exceeding the
    element-count cap rejected; an element with too many attributes and an over-long attribute
    value rejected.
34. **A decompression bomb is refused without materializing.** A block declaring a small
    geometry-implied size but inflating to many megabytes is rejected, and the test asserts the
    allocation never happens. Run for zlib, LZ4 and the zstd declared window.
35. **Fuzzing, per § Fuzzing.** Every target exists and builds. Each asserts no panic,
    termination, and total allocation bounded as § Fuzzing states, at the `ALLOC_MULTIPLE`
    that section fixes and under the one-way condition it attaches to changing it — so an
    allocation failure is graded as a defect in the allocation until the buffer that
    allocated is named. The committed corpus and every past crash artifact replay green on
    **stable** as ordinary tests in the `build-test` lane; the exploratory nightly lane runs
    on schedule and publishes crash artifacts. A panic found by the fuzzer is a release
    blocker — it falsifies invariant I5.
36. **The banned-construct grep covers dependencies too.** It runs over the whole vendored
    tree transitively, scoped to the packages `cargo tree --edges normal` reports — `cargo
    vendor` has no `--no-dev` flag — which is what extending the ban to any crate that uses
    those helpers internally asks for. A hit anywhere is a release blocker pending review of
    whether that code sits on a decode path; § Dependencies records the one package that
    review has cleared, and the lane fails if that package leaves the graph.
37. **No `unsafe`.** `#![forbid(unsafe_code)]` compiles.
38. **Peak decode memory meets the stated target.** A benchmark decodes a 25 MP 16-bit FITS
    fixture through `read_image_into` and asserts peak resident memory stays **within 1.25× of
    the destination buffer**, against the ~2.05× a non-streaming path costs (§ What streaming
    is actually worth). The margin is loose enough to absorb allocator behaviour and a decode
    buffer, tight enough that materializing the file would fail it — without it, an
    implementation that buffers the whole file passes every other criterion while missing the
    entire point of tier 2. A **second** fixture covers the compressed case: a single-block
    LZ4 `UInt16` XISF frame, whose peak is stored block + decompressed block + destination,
    at **2.6× the destination buffer**. The model gives 2.0× — stored block, decompressed
    block and destination at 2 + 2 + 4 bytes per pixel — and the threshold carries the
    remaining margin for the allocator and the compressed block's excess over its
    uncompressed size, which is bounded only by the stored-block cap. It is
    deliberately worse than the FITS figure because that is what this codec costs; a
    threshold no implementation can fail would defeat the criterion's purpose. Two forms, and
    only one of them is a gate: an **allocation** bound runs on every push, using the counting
    allocator the fuzz crate already needs, because a check that does not run per push lets the
    regression it exists to catch merge green. The **resident-memory** measurement is invoked
    by hand, beside the corpus run, because it needs a quiet machine to mean anything and a
    shared CI runner is not one — a threshold widened until a noisy runner cannot fail it
    would stop being a measurement.

### Hygiene

39. **No committed frame carries observatory coordinates.** Fixtures are synthetic or
    scrubbed; no site coordinates at real precision and no real `DATE-OBS` paired with them.
    The name list is the load-bearing part, because the obvious one is wrong: measured against
    the corpus, only 5 of 30 FITS variants use `SITELAT`/`SITELONG`, while ten more carry the
    same coordinates at the same precision under **`LAT-OBS`/`LONG-OBS`**, and the XISF surface
    is a *property* — **`Observation:Location:Latitude`/`:Longitude`**, in 540 of 1080
    variants. A grep keyed only on the FITS spellings passes full-precision site coordinates
    through. The list is `SITELAT`, `SITELONG`, `LAT-OBS`, `LONG-OBS`, `OBSGEO-*`,
    `Observation:Location:*`, plus `OBSERVER`, `TELESCOP`, `INSTRUME` and `OBJECT`.
    `test-data/` stays gitignored and the corpus lives outside the repository entirely. A CI
    grep enforces this over committed fixtures.
40. **No machine-local absolute path** appears in any committed file.
41. **Written from the published specifications alone.** `reference/` remains gitignored but
    for its README; no `pixinsight`/`pcl`/`pleiades` in crate, module or binary names.
42. **Documentation.** `#![warn(missing_docs)]` clean. The crate-level documentation states
    the normalization form and why, the report-don't-interpret rule, and the streaming
    granularity table. `docs/intentional-patterns.md` exists and covers the normalization line,
    so a reviewer or linter that wants to simplify it finds the argument first.

### Invariants

Most criteria check that a particular behavior is right. These five are different: they are
properties the design promises to hold **everywhere**, so a single counter-example anywhere
in the crate falsifies them. Each maps to named tests, and the implementation materializes
that mapping as a coverage map at `tests/COVERAGE.md`. Criteria not listed here are ordinary
behavioral checks and are not owned by an invariant.

| Invariant | Criteria that check it |
| --- | --- |
| **I1 — One normalization.** Every sample from every container passes through the same primitive; no format has its own arithmetic. | *Exhaustive `UInt16`*, *Exhaustive `UInt8`*, *Cross-format bit-identity* |
| **I2 — Delivery does not change bits.** Streamed, chunked and whole-buffer decode produce identical output. | *Streaming equals whole-buffer* |
| **I3 — No silent transformation.** Pixels are delivered in stored order with no geometric transform and no pedestal applied; nothing but normalization — including its saturation step — touches a sample value. | *Report-don't-interpret is observable*, *`PEDESTAL` and XISF `offset` change no pixel*, *Non-finite handling is total* — the last is what actually checks the saturation clause |
| **I4 — No allocation from an *unvalidated* declared size.** Every pixel buffer is sized from validated geometry, and declared sizes are cross-checks only. The one buffer that grows toward a declared length — the XML header — grows incrementally under a cap, never pre-sized. | *Adversarial suite*, *Every cap has a test that trips it*, *A decompression bomb is refused*, *Fuzzing* |
| **I5 — Malformed input errors, never aborts.** No input, however hostile, causes a panic, a hang, or unbounded allocation. | *Adversarial suite*, *Caller misuse is an error, not a panic*, *The XML guards*, *Fuzzing* |

## Detailed Implementation

This is a decision record, not an interface specification, and it is deliberately short:
the specifications say how FITS and XISF are laid out, and the criteria above say what
must be observable. What follows is only what neither of those settles. Choices not named
here are the implementer's.

### Module layout

The file layout is the implementer's, with three exceptions that carry decisions:

| Path | Why it is named here |
| --- | --- |
| `src/normalize.rs` | **Layer 2 lives alone.** It holds the three-step primitive and the range type, and contains no `use` of any format module. That import ban is the mechanical enforcement of invariant I1: a format cannot grow its own arithmetic without the dependency edge becoming visible in review. It is also the file the exhaustive tests target, which is why it must not depend on I/O |
| `docs/intentional-patterns.md` | Why the normalization line looks the way it does. Written **before** the first review, not after — its whole purpose is to be found by the next person who wants to simplify that line |
| `tests/COVERAGE.md` | The invariant-to-test map required by § Invariants |

### Build order

One ordering constraint is absolute:

> **`normalize.rs` and its exhaustive tests come first, before any file format exists.**
> The numeric core is the part that is expensive to get wrong and cheap to prove, and
> proving it against a pure function is far easier than proving it through a decoder. Every
> numeric-correctness criterion passes before a single byte of FITS is parsed.

**The rest of v1 is larger than any one consumer needs, so it is phased.** The minimum
viable slice is FITS `BITPIX = 16`, `NAXIS = 2`, the unsigned convention, and monolithic
`UInt16` XISF; everything beyond that is breadth this crate takes on because it is a shared
library rather than one consumer's loader. The phases are ordered so each ends where a
consumer can build against it:

| Phase | Ends when | Why here |
| --- | --- | --- |
| 1 — the primitive | `normalize.rs` and every numeric criterion pass | Above |
| 2 — FITS to the minimum viable envelope | Tier 1 and tier 2 decode a `BITPIX = 16` unsigned-convention frame, with the caps, the error classes and the no-panic contract already in force | This is the whole of that slice; a consumer can start building against it while the rest lands |
| 3 — XISF to the same envelope | Uncompressed and `zlib` `UInt16` attachment blocks, plus header, keywords and properties | The corpus masters are uncompressed, so this reaches real files early |
| 4 — breadth | The remaining codecs, checksums, `subblocks`, both embedded encodings, multi-image and `Reference` walks, `INHERIT`, the remaining sample formats | Each is additive and independently testable; none changes a shape settled in 1–3 |
| 5 — hardening to the stated bar | Fuzz targets, the full adversarial port, the memory criteria, the CI lanes | Needs the surface to exist first, and is a release gate rather than a milestone |

Phase order is a decision; sequencing *inside* a phase is the implementer's.

### Decisions the implementer must not silently change

**The normalization primitive.** Three steps, in order, casts exactly as written in
§ Normalization, which is its single home. No `mul_add`. No division per sample. No
separate whole-image fast path.

**Checksum before decompression, always.** XISF §10.6.1 is explicit that decompressing a
block that failed verification is exploitable. The pipeline order is fixed: read the stored
block → verify → decompress → unshuffle → de-interleave → normalize. Nothing may be
reordered to save a pass.

**Keyword storage.** An ordered list of `(name, value, comment, origin)` preserving document
order and duplicates. `origin` distinguishes a card the image itself carries from one
inherited from a FITS primary header or reached through an XISF `Reference`; without it the
`INHERIT` rule's promise that a caller can tell them apart is unimplementable. Lookup is by
**exact** byte match and returns the **first** match in that stored order, so an extension's
own card wins over one inherited from the primary header — the live case, since both
headers' cards are always reported and an inherited `EXPTIME` or `DATE-OBS` sits behind the
extension's. Every occurrence stays reachable through the list, which is what the *Keyword
lookup does not case-fold* criterion requires of `HISTORY` and `COMMENT`. Values are stored
as the text in the file, with FITS quoting removed and trailing blanks stripped per the
standard's character-string value rules — and **not** reformatted, because re-rendering a
number through a formatter can lose digits and the consumer parses these itself. A
case-folding convenience is a consumer's adapter, not this crate's lookup.

**Folding is a rule about the assembled keyword list, not about FITS cards.** `CONTINUE`
chains and `HIERARCH` names are folded wherever they appear, including across a run of XISF
`FITSKeyword` elements — nothing in XISF forbids a writer from serializing either, §11.6
asks for FITS-transparent access, and the FITS version it names predates `CONTINUE`'s
standardization, which makes improvised spellings *more* likely there rather than less.
Folding on one surface only would make the same long string read differently through the
two containers, which is what the *A keyword reads the same from either container*
criterion forbids.

**A keyword means the same thing whichever container it came from.** XISF stores FITS
keywords with their FITS quoting intact — the specification's own example is
`value="'2012-03-15T02:55:15'"`, single quotes and all — so the unquoting rule is applied to
an XISF `FITSKeyword`'s `value` attribute exactly as to a FITS card. §11.6 asks for
precisely this: a FITS-compatible decoder "*must* load all existing FITSKeyword elements and
give access to them transparently, as if the original data were stored in the FITS format".
Keyword *names* are trimmed of the space-filling FITS mandates. `HISTORY` and `COMMENT`
carry an empty `value` by specification and their text lives in `comment`.

**Geometry lives in struct fields, never in keyword lookup.** A consumer must not read
`NAXIS1` to learn the width, because an XISF frame need not carry that keyword at all. The
structural cards themselves remain in `keywords()`, because they are keywords like any others
and consumers read them by name — the rule forbids *deriving geometry from* them,
not reporting them. Correspondingly, `astroframe` **never synthesizes** a keyword that was
not in the file: an XISF frame's keyword list is exactly the `FITSKeyword` elements *that
image* supplies, its own children plus root-level ones reaching it through a `Reference`. A
root-level keyword referenced only by image 2 does not appear in image 1's `keywords()`, and
one with a `uid` that nothing references appears nowhere. In a multi-image file, two images'
keyword sets are different claims.

### FITS decisions

Everything below is a choice the FITS Standard does not make for us, or a hazard whose cost
of getting it wrong is silent. Structure, card layout, the data-unit size formula and the
value grammar are in FITS 4.0 and are not restated here. Each row is pinned by the *Every
FITS decision* criterion.

| Decision | Basis, and why it is a decision |
| --- | --- |
| **FITS is a multi-image format here, like XISF.** `next_image()` starts at the primary when it holds an image and otherwise advances to the first `XTENSION = 'IMAGE'` that holds one; each call advances to the next | A file may legitimately interleave tables between images, so non-image extensions are **skipped**, not errors — refusing the source over one would reject ordinary MEF files. An `IMAGE` extension declaring `NAXIS = 0` is skipped with them, since it holds no image either; § Errors → Where a decline surfaces settles that against the `NAXIS = 0` primary and against `NAXISn = 0` |
| **Data units are skipped using the full size formula**, `\|BITPIX\|/8 × GCOUNT × (PCOUNT + Π NAXISᵢ)`, rounded up to the 2880-byte block boundary, taking `PCOUNT = 0`, `GCOUNT = 1` in the primary | Named because the naive `BITPIX × NAXIS*` form lands mid-file on any heap-carrying `BINTABLE` and misparses everything after it. `PCOUNT` is mandatory in every extension header and carries a `BINTABLE`'s heap size. This is also the prerequisite for recognizing a tile-compressed file at all. The primary's `PCOUNT = 0`, `GCOUNT = 1` holds everywhere except under `GROUPS = T`, where §6.1.1 makes both mandatory and runs the axis product over `NAXIS2`…`NAXISn` — `NAXIS1` is fixed at 0 there, so the ordinary product is zero and the ordinary reading sizes the random-groups data unit at nothing. That position is declined, but the walk still steps over it to reach what follows, and a zero-sized step lands inside the group data |
| **A `BINTABLE` with `ZIMAGE = T` is a declined position, not an aborted source.** `header()` reports the geometry the `Z*` keywords declare, `granularity()` reports `WholeImage`, any pixel call is `Unsupported` | Commits v1 to lexing `ZIMAGE`, `ZBITPIX`, `ZNAXIS` and `ZNAXISn` — a small contained cost, accepted so the second-pass dispatch point in § The extension path has somewhere to attach. A real tile-compressed file is the ordinary shape of this: primary `NAXIS = 0`, no image extension, one such `BINTABLE` |
| **Signed `BITPIX` is handled entirely by the `BSCALE`/`BZERO` step**; there is no separate signed path, and the unsigned convention is not special-cased — it falls out | Its mirror image, the signed-byte convention (`BITPIX = 8`, `BZERO = -128`), is *not* the unsigned convention and so gets no normalized output; native samples decode normally and `with_bounds` covers the rest. Named because it is the one adjacent convention a reader will look for |
| **FITS pixel data is big-endian two's-complement, always** | The format carries no attribute to read and no default to infer, so it is pinned here for the same reason XISF's little-endian default is: getting it wrong corrupts every sample silently rather than raising an error |
| **`ROWORDER` is read into `{TopDown, BottomUp, Unspecified, Other(Arc<str>)}` and applied to nothing.** Recognition is case-insensitive and trims surrounding whitespace | `ROWORDER` is a convention, not part of FITS 4.0, so unrecognized values are expected and `Other` reports them verbatim; `Unspecified` means the keyword was absent, which is a different fact. The spelling normalization is a correctness rule, not a convenience: it is what stops the consumer's envelope predicate from admitting a flipped frame through a spelling variant |
| **`NAXIS = 3` is read as channels**, under the same caps as everything else | A spectral or temporal cube is structurally indistinguishable from a colour image in FITS, so `NAXIS3 = 4000` would otherwise become a 4000-channel image; the caps bound it and a cube exceeding them is `LimitExceeded`. Deliberately more permissive than the XISF side, where higher-dimensional geometry is `Unsupported` outright: XISF states its dimensionality explicitly and can be refused precisely, whereas refusing all FITS `NAXIS = 3` would reject ordinary RGB frames |
| **Both headers' cards are always reported** when the reader advances to an image extension — the extension's followed by the primary's, each tagged by `origin`. **Reporting is never gated on `INHERIT`** | Real archive frames put `DATE-OBS`, `EXPTIME` and `EGAIN` in the primary and frequently omit `INHERIT`; gating the report would lose a consumer its entire keyword list on exactly those files |
| **`INHERIT` gates *application*, never reporting, and only of the cards that change what a pixel means**: `BSCALE`, `BZERO`, `BLANK`, `ROWORDER`. **The extension's own card always wins.** Two of those four are gated by a shared lookup and two are not, which is an implementation shape rather than a second rule: `BLANK` is *reported and never applied* (its own row below), so there is nothing for a gate to withhold and the crate resolves it nowhere; and `ROWORDER` is gated beside that lookup rather than through it, because it is the one of the four whose **text** is reported, and re-resolving it per image position built a copy of an assembled keyword value per extension. `Decoder::primary_row_order` classifies the primary's once for the source instead. The observable rule is the same for all four. Under `INHERIT = T`, a primary card is applied only for a card the extension header does not carry itself — inheritance fills gaps and never overrides — and the test is **per card**, so a mixed pairing is reachable and legitimate: an extension carrying `BSCALE` but no `BZERO` applies its own `BSCALE` beside the primary's `BZERO`. Under `INHERIT = F`, and equally when the extension carries no `INHERIT` card at all, no primary card is applied: the extension's own values stand, and where it carries none the FITS defaults do (`BSCALE = 1`, `BZERO = 0`). `scaling()` reports the pair actually applied, so a mixture is visible rather than inferred. An `INHERIT` card in a **primary** header gates nothing and is reported like any other keyword; in an extension header it is honoured wherever it appears | The convention's provenance, where the precedence rule comes from, why application narrows to those four cards, and why the primary-header and placement leniencies are leniencies: § Where the `INHERIT` rule comes from, beneath this table |
| **`BLANK` is reported and no sample is substituted** | Deliberately *not* symmetric with NaN preservation: a NaN survives normalization intact, whereas a `BLANK` integer normalizes to an ordinary value and — at `UInt32`/`UInt64` widths where the map is lossy — cannot be recovered from the normalized output. A consumer needing blank masking reads the keyword and applies it against native samples, the only place the information is still exact |
| **`CHECKSUM`/`DATASUM` are reported and not verified** | Unlike XISF's block checksums, FITS makes these advisory rather than mandatory-when-present |
| **`CONTINUE` and `HIERARCH` are the one folding exception, and both are folded.** §4.2.1.2's two edge cases are honoured rather than smoothed over: a value ending in `&` that is *not* immediately followed by a conforming `CONTINUE` record keeps the `&` as a literal final character, and an orphaned `CONTINUE` record is interpreted as commentary text | Neither fabricates a name: a `CONTINUE` chain carries its name on the first card and `&`-terminates each continued value, so only the *value* is assembled, and leaving it literal returns a value truncated at the card boundary ending in `&`; a `HIERARCH` card carries its full multi-word name in the card itself, so folding yields `ESO DET EXP` where *not* folding gives every such card the name `HIERARCH` and collides them all. FITS 4.0 §4.2.1.2 makes `CONTINUE` standard and defines both edge cases, which are reachable on real files; `HIERARCH` appears nowhere in FITS 4.0, so it is an ESO convention and the collision argument for folding it is this design's own. Public BSD-3-Clause FITS libraries fold both, so **on conforming input** this is not a regression against established practice. Their fold is typically unconditional — a character is stripped from the previous value without checking that the value ends in `&`, and an orphaned `CONTINUE` is not treated as commentary — so on non-conforming input this design is stricter and correct where they are not |

**Where the `INHERIT` rule comes from.** `INHERIT` is a *reserved* keyword (§4.4.2.6)
whose semantics live in Appendix K, which the standard opens by stating is not part of
the Standard and is included for informational purposes — a different provenance from
`ROWORDER`, which appears nowhere in FITS 4.0 at all. Appendix K is where the precedence
rule comes from: a keyword present in both headers takes the extension's value. It also
confines its own recommendation to units with a null primary array precisely so that
array-specific keywords such as `BSCALE` and `BZERO` are not inherited — the `INHERIT`
row's exact hazard, named by the specification. Narrowing *application* to the four
pixel-meaning cards is this design's decision, which Appendix K leaves open by putting
the merging mechanism in the application's hands; applying a primary's `BSCALE` over an
extension's own would rewrite every pixel and move the frame between "unsigned
convention" and "no normalized output", the silent plausible repair this design refuses
everywhere else. Appendix K forbids `INHERIT` in a primary header, so a card found there
is data rather than an instruction, and its "immediately after the mandatory keywords"
placement is not enforced, because refusing a file over a placement that changes no
meaning is the silent rejection this design refuses. The split is report-don't-interpret
exactly: report everything, let the convention decide only what is *acted on*.

**Header character set.** FITS 4.0 restricts header cards to ASCII `0x20`–`0x7E` and real
writers violate it: non-ASCII in `COMMENT` and `HISTORY` text is common enough that
rejecting it outright would reject ordinary files. The rule is split by **where** the
offending byte sits, and the keyword-name row is called out because it is the position an
implementer meets first and the easiest to leave undecided:

| Where a byte outside `0x20`–`0x7E` appears | Result |
| --- | --- |
| In the **keyword-name** field — bytes 0–7, or, for a `HIERARCH` card, everything up to the `=` | **Hard error.** A name that cannot be represented cannot be matched, and `get` promises exact matching. The `HIERARCH` extension is explicit because that convention carries its name outside the first eight bytes, where the comment rule would otherwise tolerate it and then use it as a lookup key |
| In a **value field** | **Hard error.** A value that cannot be read as the standard defines it cannot be trusted downstream |
| In a card's **comment** field, or anywhere in a commentary (`COMMENT`/`HISTORY`) card | Tolerated. The text is decoded lossily, so an invalid UTF-8 sequence becomes a replacement character rather than a failed parse. Control bytes below `0x20` and `0x7F` are valid UTF-8 and pass through unchanged — the tolerance is about not failing the frame, not about sanitizing text |

The tolerant half of that split is what real files force; the strict half is what exact
keyword matching forces. A decoder needing only a fixed set of facts could drop the whole
card on any tolerated non-ASCII instead, which this crate cannot do, because it promises to
report what the file says.

### XISF decisions

The same rule applies: XISF 1.0 is cited, not restated. Element layout, attribute lists,
the scalar grammar of §8.3 and the block location syntax of §10.3 are in the specification.
What follows is what it leaves open, what this crate pins because a wrong guess is silent,
and where this design knowingly diverges. Each row is pinned by the *Every XISF decision*
criterion.

| Decision | Basis, and why it is a decision |
| --- | --- |
| **The preamble's four reserved bytes are ignored**, and the 65-byte minimum header length is enforced | §9.2's "shall be zero" binds encoders and places no obligation on readers; the specification's own validity check inspects only the signature and that minimum. Ignoring them is plain conformance rather than leniency, and the minimum is the cheapest of the malformed-input rejections |
| **Both `attachment:` and `attached:` are accepted** | `attachment:` is normative (§10.3) but four of the specification's own examples write `attached:`, and a writer that followed the examples produces files that are otherwise valid. The spellings cannot be confused with each other or with any other location form. A divergence from prior art |
| **A geometry with any zero-length axis is `Malformed`** | §8.5.1 calls such a thing an *empty image* and states that empty images cannot be serialized in an XISF unit — a file-validity rule rather than a scope decision |
| **An `<Image>` with no `location` attribute is `Malformed`** | §10 requires a block's "location **and role** … to be completely and unambiguously defined by the unique XISF header" and §11.5 requires an image's pixels to be a single data block, so this follows from the specification even though §11.5.1's attribute list does not name it. No ported fixture covers it, so it needs a net-new case |
| **Geometry is `dim_1:…:dim_N:channel-count`, and this version supports N = 2 exactly** | Fewer than two fields is `Malformed`; exactly two is a valid one-dimensional image and `Unsupported`; four or more is a valid higher-dimensional image and `Unsupported`. Distinguishing the malformed case from the two declined ones is the point: only the first says the file is broken |
| **`byteOrder` absent means little-endian** (§10.4) | Pinned alongside the other silent-failure defaults: a wrong guess corrupts every sample rather than producing a visible error, and big-endian is the plausible wrong guess by analogy with FITS |
| **The root `version` attribute is checked; anything but `1.0` is `Unsupported`** | §9.5 makes it mandatory, and a later version may redefine what this crate reads |
| **The header is parsed namespace-aware, matching elements by local name — within the XISF namespace when the document declares one, and unconditionally when it does not** | §9.5 says the root *should* carry the namespace, so a prefixed serialization is legal and `quick-xml`'s plain reader would fail to match it; requiring the namespace would reject the conforming files that *should* permits, while matching local names unconditionally would confuse XISF's `Reference` (§11.13) with the identically-named element inside an XML-DSig `Signature` subtree. Parsing stops at `</xisf>` anyway, so that subtree is never reached — a consequence of the rule, not a substitute for it. A root element in some *other* namespace is `Malformed` at construction rather than a document that matches no `Image` and walks zero images, which would be the silent loss this design refuses |
| **XML entity references are resolved; "verbatim" means after unescaping.** Duplicate attributes on one element are rejected rather than last-wins | `quick-xml` does not unescape automatically, so this is a decision rather than a default. A consumer comparing a keyword value against a string should not have to know how the writer chose to escape it. A `String` property's *whitespace* is still preserved exactly — unescaping changes entity syntax, not spacing |
| **Plain-text scalars follow §8.3, and one specification defect is tolerated rather than reproduced**: `0`, `+0` and `-0` are accepted as integers | §8.3's four traps for a naive parser are surrounding whitespace that *must* be ignored (§8.3.4, so `geometry=" 4096 : 2160 : 1 "` is valid), leading `+`/`-` even where the field is conceptually unsigned (§8.3.1, so a sign is parsed then range-checked), binary/octal/hex integer forms (§8.3.2), and `NaN`/`+Inf`/`-Inf` float spellings (§8.3.3 — which is how a `bounds nan` file comes to exist). §8.3.1's integer regex admits no decimal spelling of zero at all, yet `attachment:0:…` is a real and necessary location |
| **Header encoding is UTF-8 (§9.5); invalid UTF-8 is `Malformed` and a declared non-UTF-8 encoding is `Unsupported`. A missing XML declaration is tolerated** | `quick-xml` is built without its transcoding feature and guessing would be worse than refusing. The missing declaration makes the header invalid by §9.5 and is tolerated anyway, as deliberate leniency toward real writers rather than as a conformance claim; nothing in decoding depends on it |
| **The XML header may not be a well-formed standalone document, and that is fine** | A signed unit places its `<Signature>` element *after* `</xisf>` (§9.5), so the header buffer has two roots. Pull-parsing naturally stops caring once the `xisf` element closes, which is what makes a signed unit decode normally even though signature verification is out of scope |
| **`Metadata`'s absence is tolerated** | The specification requires it but defines no failure mode, and a decode-only library gains nothing by refusing |
| **`subblocks` without `compression` is `Malformed` — this crate's decision, not a spec rule** | §10.6's "*must* appear along with the compression attribute" is conditional: it governs the case where a block exceeds a codec's input limit, and the following paragraph makes the attribute optional otherwise. The specification never contemplates `subblocks` on an uncompressed block, so it neither permits nor forbids it; refusing is the same call made everywhere else here, since the attribute describes how compressed data was split and on an uncompressed block it describes nothing. The granularity floors accordingly never pair `subblocks` with an uncompressed codec |
| **Three checks are added to the subblock list**: the count is capped, the declared compressed lengths must sum to the stored block size, and the declared uncompressed lengths must sum to the geometry-implied size. All three run before any allocation | §10.6 requires no validation of the list and explicitly sets no upper limit on the number of subblocks. Without these the attribute is a cheap amplification vector the element-count cap does not cover, because the whole list is one attribute string rather than elements |
| **Checksums are verified for every block whose contents are actually read** — which makes tier 1 free for `attachment` blocks and **not** free for `embedded` ones, whose contents are read during header parse and whose digest is therefore verified at construction | §10.5 permits on-demand verification. A mismatch returns `ChecksumMismatch`: §10.5 tells a decoder to warn and let *the user* choose whether to continue, and for a library the caller is that user; the same section adds that an unattended batch decoder should not load a failing unit, which is this crate's primary setting |
| **All five algorithms are supported, not the mandatory one alone**: `sha-1` (also `sha1`), `sha-256` (`sha256`), `sha-512` (`sha512`), `sha3-256`, `sha3-512`, digests lowercase Base16 | §10.5 makes SHA-1 mandatory for a decoder claiming checksum support and the other four optional, so a cheaper sha1-only build would be conformant. Three hash crates is a small price beside a feature matrix where a file's decodability depends on which digest its writer chose |
| **`item-size` comes from the compression attribute's mandatory third field and is never derived from `sampleFormat`.** `0` is rejected; `1` is a valid no-op; a value exceeding the block length is `Malformed`; a trailing partial item is copied through unshuffled | §10.6.4/§10.6.6/§10.6.8 make the field mandatory, so a `+sh` codec missing it is `Malformed` rather than a case for inference — and §10.6 defines `item-size` only as "the length in bytes of a data item", never tying it to the sample width. That decoupling is what makes a trailing partial item reachable on a conforming file (a three-sample `UInt16` block with a legal `item-size="4"` is six bytes with two left over); the planes are defined over "subsets of equally significant bytes", which exist only for complete items, so a partial item belongs to no plane and passes through as stored. Rejecting it would refuse a file this design has just called conforming |
| **The unshuffle is fused into the per-sample read**, so no separate unshuffled buffer exists | A decision, not an optimization detail: it is what keeps `Block` granularity's peak at one block rather than two |
| **For an embedded block, `compression` and `subblocks` live on the child `<Data>` element**, not on the element that serializes the block (§10.6) | For every other location mode they live on the serializing element itself. Reading them from the wrong element yields a block that looks uncompressed and decodes to noise — a failure no synthetic round-trip catches unless a fixture exercises embedded-plus-compressed. One does |
| **Embedded blocks come in two encodings, `base64` and lowercase `hex`** (§10.3), and both are supported. An unknown encoding is `Malformed`; an uppercase Base16 spelling is rejected rather than accepted leniently | The Base16 half is net-new work rather than a port — that decoder implements `base64` only — so its test is the only evidence it will get. The specification is not silent on digit case, so there is nothing to guess at |
| **Whitespace is stripped before Base64/Base16 decode, at the two decode sites and never at the reader** | §10.3 says white space "is irrelevant and *must* be ignored" for both encodings and the specification's own embedded example is line-wrapped — but §11.1.6 says the opposite for a `String` property's character data, whose whitespace "a compliant decoder must preserve". Both surfaces are read by the same parser, so `quick-xml`'s obvious `trim_text(true)` silently corrupts every string property. "Whitespace" here is the four characters XML itself defines — space, tab, CR, LF — not Rust's Unicode-aware `char::is_whitespace`, which would also accept spaces no conforming writer emits and a hostile file might |
| **Character data is assembled to the element's end before either rule applies** | `quick-xml` reports CDATA as a distinct event, and a run interrupted by an entity reference, a comment or a CDATA boundary arrives as several events. A parser that reads one text event truncates `String` property values and rejects CDATA-wrapped embedded blocks — both legal XML, and a `String` property containing `<` is exactly why a writer reaches for CDATA |
| **`inline` is not a legal location for image pixel data; an `<Image location="inline:…">` is `Malformed`** | §11.5 is explicit that an `Image` element cannot serialize pixel data as an inline block, because `Image` may have child elements. §7.2's baseline bullet lists inline among the locations a decoder reads, but that is about data blocks in general — properties and thumbnails among them — and §11.5 is the specific rule. `embedded` is the legal in-header spelling and is supported |
| **A negative `offset` is `Malformed`** — the one place report-don't-interpret does not extend to reporting | §11.5.2 defines `offset` as a scalar whose value *"must be greater than or equal to zero"*; the constraint lives there, not in §8.3.1's grammar, and `NaN`/`-Inf` spellings are expressible via §8.3.3. The attribute is not merely unusual, it is outside the range the specification defines for it |
| **An `attachment` position must lie beyond the header region** — at least `16 + headerLength` | Nothing else rules this out, and a declared position of, say, 40 on a seekable source would hand the caller the XML header's own bytes as pixel samples: a decode that looks plausible and is fabricated. One comparison closes it |
| **`Reference` elements are resolved by `uid` lookup over the whole parsed header, in a second pass**, for every element this crate reports that the specification permits at the root — `FITSKeyword`, `Property`, `ColorFilterArray`, `Resolution`, `DisplayFunction`, and `Image` itself. A `Reference` to anything else is ignored. **The resulting order is pinned: document order, with a referenced element taking the position of its `Reference`** | Forward references are legal — §11.13 requires only that the target be defined in the same unit, and the specification's own examples define the target *after* the `Reference` — so a one-pass backward-only resolver would silently drop metadata on conforming files. The header is fully buffered anyway, so the second pass is free. Resolution is exactly one hop and needs no cycle detection: §11.13 makes `Reference` the only core element that cannot itself carry a `uid`, so chains are not expressible. The specification does not define the order, so this crate pins one |
| **`FITSKeyword` must be a child of an `Image` or of the root** (§11.6); one inside `Metadata` is ignored rather than reported. Its `comment` attribute is mandatory (§11.6.1), so the comment field is always present for XISF-sourced keywords and absent only for FITS cards carrying none | A non-conforming placement is attached to no image, and reporting it against an arbitrary one would invent an association the file does not make |
| **Property identifiers are reported verbatim and never validated as tokens** | An implementer who validates ids against a well-formed `Namespace:Path` grammar will reject real files: a space-bearing id such as `"Instrument: colorFlag"` has been reported in the wild, though it appears nowhere in this corpus, whose masters' 368 properties across 112 distinct ids are all well-formed |
| **`Thumbnail` elements are skipped and their data blocks stepped over**, bounded by the **`Skipped block bytes`** cap rather than by the stored-block cap | §11.12 gives a `Thumbnail` the shape of an `Image` with extra restrictions, and it may sit under an `Image` or at the root. It is not an image this crate reports, so `next_image()` never yields one — but a sequential source still has to skip its attached block to reach what follows. The stored-block cap is the wrong instrument, being phrased against the *current image's* geometry, while a thumbnail has its own geometry and may sit at the root with no current image at all. The skip is all of it: **a thumbnail's declared block is not validated**, and that is a consequence of § Hardening's rule that a declared block offset is never validated during the header phase, not an omission. A thumbnail is never an image occurrence, so nothing in the pixel phase opens its block and there is no later place to check it either — and checking it at construction was tried and reverted: every XISF file in the corpus carries a `Thumbnail`, so as a size-capped prefix every one of them became `Malformed`, which is tier 1's whole purpose defeated. On a known-length source the skip is a seek that transfers nothing, so an over-large declaration costs nothing; on a **length-unknown** source the cap is what terminates the skip — without it a declared 2⁶³-byte thumbnail on a pipe is an unbounded read, which is the hang invariant I5 names |
| **"Declined" means two different things, and the difference is observable.** For a *frame-level* capability — a sample format, a colour space, a geometry, a location — declining is an `Unsupported` **error**. For an *element* this crate does not read, declining is **silent non-reporting**: the element is skipped, the frame decodes, nothing is raised | The rule is that an element never fails a frame it does not prevent decoding. It is load-bearing for two cases the specification makes common: `RGBWorkingSpace` appears in §11.13's own worked example and PixInsight writes it routinely, so treating it as frame-level would refuse a large share of real RGB files for a colour-management element that has nothing to do with pixels; and a block-valued `String` property is skipped rather than fatal, which is the honest cost of that deferral |
| **The core elements this crate meets and does not read are dispositioned explicitly** rather than left to fall through the ignore-unknown rule: `Resolution` (§11.11) and `DisplayFunction` (§11.9) are **reported** as attribute-valued metadata; `ICCProfile` (§11.7) and `RGBWorkingSpace` (§11.8) are **declined**; `Table` (§11.3) is declined with its properties; `Structure` (§11.2) is declined and, carrying no metadata a consumer reads, is not reported | The two colour elements exist to drive colour conversion, which is out of scope, and `ICCProfile` additionally carries a data block whose bytes are always big-endian and which §10.4 forbids from carrying `byteOrder` — a special case worth not inheriting. `DisplayFunction` is reported on report-don't-interpret grounds alone: it is metadata the file states and no consumer can recover otherwise |
| **Unknown elements and attributes are ignored — this crate's decision, not a citation.** Unknown values of the enumerations decoding *depends on* (`sampleFormat`, `pixelStorage`, `colorSpace`) are hard errors; unknown values of `imageType` and `orientation` degrade to "unknown" and are reported as text | The specification states no forward-compatibility rule anywhere, and ignoring unknowns is the only reading under which a 1.0 decoder survives a later revision adding elements. `imageType` and `orientation` are closed enumerations too (§11.5.1 Table 12, §11.5.2), so the criterion is what decoding depends on, not whether the enumeration is closed |
| **`pixelStorage` absent means `Planar`, `colorSpace` absent means `Gray`** — never inferred from channel count. **Channel count is never validated against the colour space**: channels beyond its nominal count are alpha channels and are delivered as ordinary channels | §8.5.1 adds that for images with a visual representation role the first alpha channel *should* define transparency; this crate has no visual role and assigns them none. The second half is the load-bearing one — a decoder that defaults correctly and then checks "`Gray` implies one channel" rejects three legal combinations, which is exactly what prior art does (§ Deliberate divergences from prior art) |

### Local corpus validation

A generated format matrix and a set of real integrated masters exist **on one machine**. They
are the acceptance evidence for the format support matrix, and they are **never committed and
never run in CI** — 84 GB, and real observatory data throughout. Site coordinates appear at
full precision, most of them under `LAT-OBS`/`LONG-OBS` rather than `SITELAT`/`SITELONG`, and
ride into the XISF variants as `Observation:Location:*` properties. That keyword spread is why
the hygiene grep checks the list it does rather than the obvious two names.

The matrix is five sample formats crossed with nine compression modes and four checksum modes,
over a handful of source frames, plus a CFITSIO tile-compressed family and the masters. What
makes it worth having is provenance: it was written by **production applications** —
PixInsight, AstroPixelProcessor, N.I.N.A., ASIAIR — and by **CFITSIO** as a reference
implementation, so it is a specification of what real writers emit rather than a set of
guesses. What makes it unusable as a test suite is that it exists nowhere else.

**The corpus is also the XISF path's only independent oracle, and that turns out to be its most
valuable property.** The XISF variants were produced by PixInsight *from* the FITS frames at
the corpus root, so each root frame and its variants are the same pixels written twice by two
different implementations. There is no second Rust XISF decoder to differential against, so
without this the XISF side rested on the exhaustive normalization tests plus cross-format
identity over *synthetic* fixtures — a sound argument whose premises were all this crate's own.

Every root frame is `BITPIX = 16` with `BZERO = 32768`, so the bound for each variant family is
*derived from its conversion* rather than fitted to a measurement — and three of the five are
exact:

| Variant format | Bound | Why | Measured worst |
| --- | --- | --- | --- |
| `uint16` | exact | The same 16-bit levels, normalized by the same primitive | 0 |
| `uint32` | exact | `4294967295 = 65535 × 65537`, so a level becomes `L × 65537` and `L × 65537 / (65535 × 65537)` *is* `L / 65535`. Lossless by arithmetic | 0 |
| `float32` | exact | The conversion lands on the same `f32` this crate computes. Asserted exact **because** it is exact: a tolerance here would stop the test noticing if that changed | 0 |
| `float64` | `2⁻²⁴` | One ULP of `f32` near 1.0 — the multiply-by-rounded-reciprocal divergence § Normalization pins deliberately | 5.960e-8 |
| `uint8` | `1/510` | Half of one 8-bit step, the step being `1/255` because § Normalization divides by `hi − lo` | 1.9532e-3 |

That last row is worth keeping for the reason it is right. It was written `1/512` first —
"half of 256 levels" — and the corpus rejected a set of files that were perfectly correct,
because the measured worst falls *between* `1/512` and `1/510`. Deriving a bound and then
checking it against reality is what surfaced the mistake; a tolerance fitted to the
measurement would have passed and quietly recorded a misunderstanding of the very
denominator this crate's central contract is about.

**How a developer runs it.** The corpus-backed checks are `#[ignore]`d and locate the corpus
through an environment variable, skipping cleanly when it is unset:

```text
ASTROFRAME_CORPUS=/path/to/corpus cargo test --release -- --ignored
```

**What that run proves that no fixture can:**

| Check | What it establishes |
| --- | --- |
| Every file decodes or declines cleanly | Every file in every family produces a decoded frame or a stated error class, never a panic, a hang, or a silent misread. The tile-compressed family is the honest test of that decline: every one of its files must yield a clean `Unsupported` |
| Differential FITS check against `fitsrs` | Native samples match for every pixel of every FITS variant — catching header-parsing and endianness errors that a hand-built fixture cannot, because a fixture is built by the same understanding it tests |
| Cross-entry-point agreement | Header-only geometry matches single-image decode, and the image *count* matches an all-image walk, with pixels compared across `open`, `sequential` and `seekable` |
| The masters | Most masters hold **two images**, and one holds two of *different geometry and different sample format*. That is the multi-image walk, the per-image scoping rule and the reset-on-`next_image()` rule, all in one file, on real data. Every `Float32` master carries `bounds="0:1"`, which confirms the mandatory-`bounds` rule against real writers and puts the identity-multiply saturation case on the common path rather than a hypothetical one |
| Peak resident memory | Measured against real masters of a few hundred megabytes rather than against a fixture |

**What the corpus does *not* cover, measured rather than assumed** — and therefore what the
committed fixtures must carry alone:

| Axis | Coverage |
| --- | --- |
| Sample format | `UInt8/16/32`, `Float32/64` — **no `UInt64`** |
| Codec × checksum | Complete: 9 × 4, all combinations |
| Shuffling at 8 bits | The 8-bit shuffled variants declare no `+sh` — the writer elides shuffling for 8-bit data — so **`item-size == 1` never appears** |
| `subblocks`, `byteOrder`, `pixelStorage`, `Reference`/`uid` | **Zero occurrences** in any file. `Planar` and little-endian throughout; the whole `Reference` mechanism is unobserved in practice |
| `colorSpace` | `Gray` and one channel throughout — RGB only in the masters |
| Location | Image pixels are `attachment` throughout. But `inline:base64` occurs in a third of the variants (astrometric property blocks) and `embedded` with a child `<Data>` in roughly two fifths (thumbnails), so both locations *are* exercised — in files whose `<Image>` element alone looks attachment-only |
| Images per file | One `<Image>` throughout — multi-image only in the masters |
| Ancillary elements | **Every** variant carries a `<Thumbnail>`, a `<Resolution>` and a `<Metadata>` block with scoped properties and dozens of image-scope `FITSKeyword` children, so thumbnail skipping and the three-scope property rule are exercised everywhere |

Two conclusions. The corpus grades **more** than its directory names suggest, because the
interesting content sits outside the `<Image>` element. And **`Reference` resolution has zero
instances in the whole corpus** — the machinery this document argues at length is unobserved in
practice, while the `inline:base64` property blocks that appear in a third of the variants and
every master are deferred. That allocation of effort is defensible, since the specification's
own example makes the `Reference` spelling legal and a decoder that mis-handles it silently
loses images, but the document should not imply the frequencies run the other way.

### Testing mechanics

- **Compare pixel buffers with `f32::to_bits()`**, never `==` and never an approximate
  comparison — `==` silently accepts sign-of-zero differences, which is exactly the class of
  defect endpoint tests exist to catch. Snapshot tests store bits as hex for the same reason.
  Nothing upstream will catch a one-ULP regression on this crate's behalf: prior art's own
  golden-model comparison uses an absolute epsilon of `1e-6`.
- **The synthetic FITS fixture must contain divergent levels.** A plausible-looking sample
  vector — `{0, 65535, 32768, 100, 1, 2, 3, 4, 60000, 7, 8, 9}` — contains
  **none** of the 512 levels where the multiply and divide forms differ, so a fixture built
  from it would pass against a divide-form implementation, and the test that looks like it
  guards the contract does not. Any fixture here includes at least one divergent level; 257
  is the smallest.
- **Fixtures are built byte-by-byte in the test source, not checked in as opaque blobs.**
  Every byte is visible where the assertion is, which matters more than usual here: a fixture
  nobody can see cannot be reasoned about when an exhaustive bit-comparison fails.
- **Two traps inherited with the ported fixtures.** The attachment offset depends on the
  header length, which depends on the digit count of the offset; the image-writing helpers
  there iterate to a fixed point and assert convergence, while the **adversarial** helper that
  builds most of the ported cases runs the same loop with no such assertion and silently gives
  up — so any fixture whose header text length changes during construction needs the assertion
  added, not inherited. And the sample generator there uses a deliberate short repeating cycle
  to keep LZ4 blocks compressible, because an LZ4 block compressor may signal "incompressible"
  by returning zero bytes; random test pixels therefore break an LZ4 round-trip in a way that
  looks exactly like a decoder bug.
- **Fixtures are synthetic or scrubbed**, per the *No committed frame carries observatory
  coordinates* criterion: real frame headers carry observatory coordinates at roughly 10 cm
  precision alongside timestamps, and the pair is a movement record.

### The extension path

The goal is a library that reads **everything**, reached in two passes rather than one: a
core covering the common matrix, then the esoteric families. "Deferred" is only an honest
answer if the way back is designed rather than hoped for.

**Tile-compressed FITS is the named second-pass family**, and the corpus carries a whole
family of it. It is a bigger change than a codec because it is not one: a tile-compressed
image is a `BINTABLE` extension carrying `ZIMAGE = T`, whose rows hold per-tile compressed
byte arrays, with the real geometry in `ZNAXISn` and the real sample type in `ZBITPIX`.
Reading it means a binary-table reader, per-tile decompression across five codecs, and
tile-to-row reassembly.
It lands cleanly for three reasons the core design already provides:

| What it needs | What already exists |
| --- | --- |
| Somewhere to put a second FITS decode path | `next_image()` already walks extensions and already recognizes `ZIMAGE = T` well enough to decline it — the recognition point becomes the dispatch point |
| A way to expose tiles without a new API | A tile is a chunk. The chunk contract is already "a contiguous run of one channel's samples in destination coordinates", which a tile satisfies; `Granularity` gains a `Tile` floor between `Rows` and `Block` |
| Normalization that does not change | `ZBITPIX` is an ordinary `BITPIX`, so the pinned primitive applies unchanged |

The parts that are genuinely new are the binary-table reader and the five tile codecs,
neither of which touches the layering, the caps model, or the normalization rule. That is the
test of whether a deferral was honest, and this one passes it. The same shape covers the
rest: distributed XISF adds a source that resolves sibling files, which is a source
implementation rather than a decoder change; `CIELab` adds a colour-space conversion between
layer 1 and layer 2 without touching either. Complex sample formats are the one family that
does *not* fit — they are not scalar pixel data, so they would widen the output type — which
is why that entry's reason is worded differently from the others.

## Deferred and out of scope

Questions considered during design and explicitly not answered. Each entry is a decision, not a gap: do not treat these as missing design, and do not re-litigate them without genuinely new evidence.

1. **Out of scope** — Should v1 support encoding/writing FITS or XISF?
   _Reason: No consumer writes frames; the decode contract is what carries the bit-exactness requirement, and an encoder shares none of it._
2. **Out of scope** — Should v1 support distributed XISF (`.xish` header file plus `.xisb` data-block files), and the `url(...)` / `path(...)` block locations that go with it?
   _Reason: `url(...)` and `path(...)` mean performing network fetches and filesystem traversal driven by attacker-controlled header content, which the XISF specification gives no guidance on securing; the monolithic-only decoder stays conformant without it (XISF §7.2 baseline)._
3. **Out of scope** — Should the library demosaic CFA/Bayer frames?
   _Reason: Demosaicing is an irreversible interpretation with many defensible algorithms, which makes it a consumer's policy choice under this design's report-don't-interpret rule. The mosaic itself decodes normally as a single channel, and the relevant keywords are reported._
4. **Out of scope** — Should the library interpret WCS keywords into a coordinate solution?
   _Reason: Squarely consumer policy: WCS interpretation is astrometry, not container decoding. The keywords are reported verbatim, which is everything an astrometry layer needs._
5. **Deferred** — Should the reader support random-access navigation across images/HDUs rather than forward-only advance?
   _Reason: Forward-only `next_image()` is what keeps tier-1 header decode cheap for FITS, where enumerating HDUs eagerly would mean reading past every data unit. Random access is a clean later addition for seekable sources and breaks nothing in the current shape; no consumer needs it yet._
6. **Deferred** — Should the API expose raw, undecoded block bytes below the native-sample layer?
   _Reason: The native-sample layer already gives a caller the file's own values, which is what every known consumer needs. Exposing undecoded bytes would additionally expose byte order, shuffling state and interleaving as caller concerns — re-exporting the complexity layer 1 exists to absorb._
7. **Out of scope** — Should the library verify XISF XML signatures?
   _Reason: Signature verification is optional in the XISF specification and would pull in an XML-DSig and certificate stack far larger than the decoder itself. A signed unit still decodes: its `<Signature>` element sits after `</xisf>` (§9.5) and is ignored. Block checksums, which the specification does make mandatory when present, are verified._
8. **Out of scope** — Should v1 decode `Complex32`/`Complex64` sample formats?
   _Reason: Complex samples are not scalar pixel data, so they do not fit the crate's output model at all: the native-sample type is an enum over scalar widths, and a complex variant would widen it — and every consumer matching on it — for data no consumer reads. This is a different reason from the FITS-float case, where the samples are ordinary scalars and only the normalization is undefined, so decoding natively and refusing only the normalized output loses nothing. The XISF specification additionally leaves the representable range of complex images undefined (§8.5.5), so there would be no spec-derived normalization to offer either._
9. **Deferred** — Should v1 offer reduced-cost decode paths for previews — sub-rectangle (ROI) decode, decimated or reduced-resolution decode, or reading XISF's purpose-built `Thumbnail` element?
   _Reason: The Problem section names a preview consumer, and this version gives it nothing a full decode does not — so this is recorded rather than left as an inference. ROI and decimated decode interact with every layer at once: granularity stops describing delivery, the normalization primitive gains a stride, and the chunk contract changes shape, so it is a second decode path rather than an addition to this one. Reading `Thumbnail` is far cheaper and is the natural first step if a preview consumer materializes, since the element is already parsed and skipped; it was left out only because no consumer needs it yet. Nothing here forecloses either._
10. **Out of scope** — Should v1 decode `CIELab` images, converting them to RGB?
    _Reason: Requires the full RGB-working-space conversion machinery (XISF §8.5.4.1-§8.5.4.6), which is colour science rather than container decoding. This design declines colour-space conversion generally. A baseline XISF decoder needs only Gray and RGB (§7.2)._
11. **Out of scope** — Should v1 expose a C ABI so non-Rust consumers can link the crate?
    _Reason: No consumer needs one. It would also be a second API design rather than a wrapper, flattening the borrowed-chunk API, the error enum and the sample-format enum into handles and out-parameters with ownership rules across the boundary. Nothing here blocks adding one later: the layering already separates the pure primitive from the I/O, which is the part such a surface would wrap._
12. **Deferred** — Should XISF `Property` elements whose values live in a data block (vectors, matrices, long strings) be decoded?
    _Reason: Deferred, and the earlier reason for it was wrong. That reason claimed no block-valued property had been observed carrying metadata a consumer needs. Measurement refutes it: the RGB master in the conformance corpus carries 26 such properties, all F64Vector or F64Matrix, holding the complete astrometric solution — control points, the linear transformation matrix, and the spline grids, some inline-base64 and some in attached blocks. The Problem section names a plate-solver among the motivating consumers, and that consumer gets nothing from this crate on real PixInsight output. The honest reason to defer is cost and sequencing, not absence of value: reading them means the XISF property type system for vectors and matrices, a second block-reading path, and a decision about what a vector-valued property even looks like in this crate's output. None of it is hard, none of it interacts with the pixel path, and it is the single most valuable thing to add after v1._
