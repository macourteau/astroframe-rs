# Intentional patterns

Code in this crate that looks wrong, or looks like it wants simplifying, and is neither.
If you are here because a line offended you or a linter, read the entry before changing it.

## The normalization line

`src/normalize.rs` computes a normalized sample as three steps with the casts exactly where
they are written:

```rust
let physical: f64 = bscale * raw.widen() + bzero;      // or raw.widen(), unscaled
let shifted:  f32 = (physical - self.range.lo) as f32;
let product:  f32 = shifted * self.range.k;            // k : f32 = 1.0 / ((hi - lo) as f32)
```

**The idiomatic line is `raw as f32 / 65535.0`. It compiles, it is shorter, and it is a
different function.**

Division is correctly rounded: one rounding. The form above has two — the reciprocal is
rounded when `k` is built, then the product is rounded. Measured over every input:

| Sample format | Levels where the two forms differ | Max distance |
| --- | --- | --- |
| `UInt8` | **126 of 256 (49.2%)** | 1 ULP |
| `UInt16` | 512 of 65 536 (0.781%) | 1 ULP |

At 16-bit level 257 the two give `3.921568393707e-03` (multiply) and `3.921568859369e-03`
(divide). A difference of exactly that class has been measured to shift frame medians in a
downstream measurement pipeline — 4 of 17 frames on one such comparison.

This crate performs the first floating-point rounding in any pipeline built on it, so the
decoded bits are part of the public API: a release that changes one ULP of output for an
input it previously decoded is a breaking change. `tests/normalization.rs` fails in plain numbers if the form
is changed — it asserts the *count* of divergent levels, not merely that the current
implementation matches itself.

### Five smaller things in the same lines, each load-bearing

- **`(hi - lo) as f32` — the cast is not redundant.** Narrowing the width and then taking
  the reciprocal is bit-identical to taking the reciprocal in `f64` and narrowing the
  result. Taking the reciprocal of the **un-narrowed `f64`** width is not: it disagrees on
  roughly a quarter of widths. A declared `bounds` pair can be any width at all, so this is
  reachable on ordinary files.
- **`k` is built once per image, never per sample.** Not an optimization — a per-sample
  `1.0 / width` would be the same value, but a per-sample *divide* would not.
- **Step 2 narrows to `f32`, step 3 stays there.** For a float source with a nonzero
  declared `lo`, an `f32` subtraction differs from the `f64`-then-narrow one by a ULP.
  There is no `f64` left after step 2, which is also how the clamp comes to be in `f32`.
- **No `mul_add`, and no fused helper of any kind.** An FMA rounds once where the contract
  needs two roundings. This extends to `algebraic_*`, `powi`, `to_degrees` and
  `to_radians`; the lint lane greps for all of them, across the vendored dependency tree as well as
  this crate.
- **The unscaled step-1 spelling is a genuine identity, not an arithmetic no-op.** Writing
  it as `1.0 * x + 0.0` to match the scaled path would invite someone to simplify the
  *scaled* path in the other direction. The two spellings differ on no observable output —
  the only value on which they differ is `-0.0`, whose sign step 3's zero normalization
  removes anyway.

### The saturating clamp is not `clamp()`, `min()` or `max()`

```rust
if product.is_nan() { product }
else if product < 0.0 { 0.0 }
else if product > 1.0 { 1.0 }
else { product }
```

`f32::clamp` and a `min`/`max` pair both fold NaN to a bound. A NaN pixel is a real signal —
a masked or dead pixel — and turning it into black data is quiet fabrication that propagates
into statistics as a real measurement. So NaN is preserved, and the output invariant is
"finite samples lie in `[0,1]`", not "all samples lie in `[0,1]`".

`±Infinity` does saturate, to `1.0` and `+0.0`, which is what the ordered comparisons above
give for free.

### The trailing `if clamped == 0.0 { 0.0 } else { clamped }` is not dead

It removes a negative zero. Two paths reach one and the clamp closes only the first: a
sample below `lo` gives a negative product the clamp brings to the low bound, while a stored
`-0.0` with `lo == +0.0` passes the clamp untouched, `-0.0` not being *below* `+0.0`. Every
pixel comparison in this crate's tests is `f32::to_bits()`, under which `-0.0 != +0.0`, so a
stray sign bit is a test failure a `==` comparison would have hidden.

## Header-derived text is `Arc<str>`, never `String`

Every text a header parser lifts out of a file and puts in a reported value is shared, not
owned: `Keyword`'s packed buffer, `Header::image_id` and `image_uuid`, `Cfa::pattern` and
`name`, `Property`'s `id`/`format`/`comment`, `PropertyValue::Text`, `PropertyType::Other`,
`Orientation::Other`, `ImageType::Other`, `ResolutionUnit::Other`, `DeclineReason::reason`,
`Occurrence::declared_bounds` and `BlockPlan::subblocks`. The accessors return `&str` and
`&[T]`, so most of it is invisible from outside; `PropertyValue::Text`, `PropertyType::Other`,
`Orientation::Other`, `ImageType::Other` and `ResolutionUnit::Other` are the five that a
pattern match can see.

**The reason is that a header lets one piece of text be reached many times, and the caps that
bound it do not multiply.** The reachability is most obvious in XISF, and for a long time the
rule was written as though it were XISF's alone; it is not, and the paragraph on FITS below is
what that cost. `<Reference>` resolves to a node, and § XISF decisions requires every
reference to be reported as its own occurrence — so one `<Image uid>` reached by N root-level
references is N `Header`s, one root `<Property>` or `<FITSKeyword>` reached by N in-image
references is N reported values, every root `<Metadata>` property belongs to every image, and
one root node referenced once by each of N *distinct* images is read N times over unless
something says otherwise. The multiplier is not always the reference count.
`XML element count` is 100 000 and `Attribute value length` is 1 MiB, and nothing anywhere
bounds their product: a conforming three-megabyte header can ask for a hundred gigabytes of
copies. That is invariant I5's unbounded-allocation clause on the header-only path a consumer
runs over an untrusted size-capped prefix, and it is not hypothetical — it has been found and
fixed one instance at a time eight times over.

So the rule is structural rather than per-field:

- A reported value built from header text holds `Arc<str>` (or `Arc<[T]>`). Adding a `String`
  field to `Property`, `Header`, `Occurrence`, `BlockPlan` or a `…::Other` variant reopens the
  class.
- **The rule is about text *built* per occurrence, not only about text *held* per occurrence.**
  § Fuzzing's allocation bound counts every allocation and not the peak, so a transient copy is
  as expensive as a kept one — and a "transient" copy on a branch that pushes its result is not
  transient at all. Two of the eight instances added no field to anything: an orphaned
  `CONTINUE` record rejoined its `value` and `comment` into a fresh `String` per occurrence and
  kept it, and opening a `CONTINUE` chain copied the comment attribute per occurrence for a
  continuation that mostly never came. Before writing a `to_owned()`, a `format!`, a `collect()`
  or any other allocating call inside a per-occurrence walk, ask what bounds the number of times
  that line runs. If the answer is a cap rather than the length of the input, it is this class,
  whether or not a field is involved. `read_display_function` splits five attributes into field
  vectors and holds no text at all, and it is in the cache for exactly this reason.
- **A node reached through a `Reference` is read once *per document*, and the built value is
  shared.** The memo key is the `Doc` node index — plus the `PropertyScope`, because a root
  `<Property>` reached from `<Metadata>` and again from inside an image is two different
  reported values. **The key must be document-scoped.** A memo local to a function that runs
  once per image reads a shared root node once per *image*, which is the multiplication the rule
  exists to stop with a smaller constant: N distinct images each referencing one root node at
  the `Attribute value length` cap cost 275 MB to 812 MB from a one-megabyte header, with the
  per-call memos in place and hiding it. `xisf::image::Cache` is the memo; `walk_occurrences`
  creates it and threads it through `build_occurrence`, `collect_children`,
  `metadata_properties` and `fold_records`.

  Every reader a `<Reference>` can reach must be covered by it, and the complete list is
  `fold_records` — **both** branches, the ordinary record and the orphaned `CONTINUE` —
  `read_property` from either of its two call sites, `read_cfa`, `read_resolution` and
  `read_display_function`. `build_occurrence` is the sixth, memoized by `walk_occurrences`
  itself, which is document-scoped already because the walk runs once per document. A reader
  added beside these is added to the cache in the same change.

  **A memo over records does not cover what is built from several records at once.** A
  `CONTINUE` chain's value is assembled across a run of them, so no key on one node can hold it,
  and `fold_records` runs once per *distinct* `<Image>` — `walk_occurrences`'s occurrence memo
  covers reference-reached images only. So 256 distinct images each carrying two `<Reference>`
  elements to one half-megabyte opener and one half-megabyte continuation each assembled their
  own megabyte-long `String`: 1.03 GB allocated and 264 MB held live from a 1.05 MB header,
  982× its input, with the per-record memo in place and hiding it. `close_chain` memoizes the
  assembled `Keyword` on the opening record's node and reported origin followed by each
  continuation's node in order.
- **A memo keyed on a superset of the computation's inputs is defeated by anything outside
  those inputs.** This is the general form of the mistake the entry above records, and it is
  worth more than the fix.

  The first attempt at that memo keyed the whole image's *folded list* on the whole image's
  *record sequence*. That key is exactly right for what it names: the folded list really is a
  function of the ordered records, so the memo was correct, and it hit on every fixture the
  enforcement test had. It was still no use, because the expensive object was not the list —
  it was the chain assembly, which is a function of the opener and the continuation records
  alone. Everything else in the key was free variation for an attacker: one
  `<FITSKeyword name="U00001" .../>` per image, forty bytes each, and no two images shared a
  chain that was byte-identical in both. The measured cost of those forty bytes was 11.3× the
  input becoming 982×.

  So the rule has two halves and the second is the one that gets skipped:

  - Memoize **the expensive object**, at the point it is built, not some container that
    happens to hold it.
  - Key it on **exactly what that object is a function of, and no more**. Write the inputs
    down and check them off: for an assembled chain they are the opener's text and origin and
    each continuation's text in order, so the key is those records' node indices and the
    opener's origin — a node index standing in for the text it fixes, and nothing standing in
    for anything else. A field that is in the key and not in the list is a way to miss; a
    field that is in the list and not in the key is a wrong answer. Both are silent.

  A key that is *narrower than the last one* is not the test. Narrower keys make the memo hit
  more often, which looks like progress right up to the shape that adds one more byte outside
  it. The test is whether the key and the inputs are the same set.
- **The class is not XISF's.** FITS has no `<Reference>`, and the rule was written as though
  that settled it. It does not: § FITS decisions requires an image extension to report the
  primary header's cards after its own, so the primary's list is a root list applying to every
  image exactly as root `<Metadata>` is. `FITS header cards` (4096) times `Images per source`
  (256) is a product nothing bounds, and concatenating the two lists into an owned `Vec` per
  extension held 52 MB live from a 1.07 MB input. `Header::keywords` returns a `Keywords` view
  over the two pieces for the same reason `properties()` returns one over three. And the FITS
  side has a second product beside that one — `Assembled keyword value` times `Images per
  source`, through an inherited `ROWORDER` — so one instance per format is not the end of it
  either. An argument that a multiplier does not exist is a claim about **one format**, and it
  has to be made twice.

  The cache is consulted **only for reference-reached nodes**: a direct child appears once by
  construction, so memoizing those is pure overhead. That is measured, not assumed — removing
  the gate costs the 49 000-keyword shape 8.5 MB, the 80 000-keyword image reached 256 times
  14.0 MB, and the 40 000-property one 12.9 MB.
- **A per-image list whose length is the document's is not built at all.** Sharing bounds a
  value copied per occurrence; it does nothing for a *merge*, whose contents differ per image
  even when every part of it is already shared. §11.4's root `<Metadata>` properties belong to
  every image, so an image adding one property of its own merged the whole root list into a list
  of its own: 40 000 root properties beside 256 such images allocated 3.2 GB and retained 2.2 GB
  from a 1.9 MB header, with every `Property` in it an `Arc` five times over. `Header::properties`
  returns a `Properties` view over the three pieces document order splits that merge into — the
  root properties before the image, the image's own, the root properties after it — and
  `metadata::PropertySet` carries the argument for why one split index is always enough. Before
  storing a collection built per image, ask what its length is: if the length is the input's and
  the number of them is a cap, that product is this class again and the answer is a view rather
  than a copy.
- **A value derived from a shared list is per-occurrence work even when the list is shared.**
  Making the primary's cards a `Keywords` view stops the *list* being copied; it does nothing
  for a value computed *from* it at each position. `ROWORDER` is that case — the one
  inheritable card whose text is reported rather than lexed to a number — and it multiplied the
  same way after the view was in place. Ask what a per-occurrence line derives from as well as
  what it holds.
- The public `Error` enum keeps its `String`s. `DeclineReason::to_error` allocates one, at the
  pixel call that actually raises it, which happens once — not once per occurrence.

One reported text is deliberately still owned, and it is not an oversight.
`Bounds::CallerSupplied::declared` is built by `Reader::header()` when the caller has set
`set_bounds`, so it is one allocation per call the caller made, not one per occurrence the file
declares; it is also a public field, which `Arc<str>` would change for no gain.

`RowOrder::Other` **was** the second, on the reasoning that FITS has no element reachable twice
— a card belongs to the header it was read from. That was wrong twice over. The primary
header's cards are reported by every extension, which is what the `Keywords` view is for; and
`ROWORDER` in particular is *applied* to every extension under `INHERIT = T`, so its text was
classified once per image position. The size of it is the second surprise: a `ROWORDER` value is
a **keyword** value, bounded by `Assembled keyword value` rather than by one card's eighty
bytes, because a `CONTINUE` chain assembles it. A 240 KB assembled value across 256 extensions
cost 189 MB, 64 MB of it held live, from a 1.06 MB input. `RowOrder::Other` holds `Arc<str>` and
`Decoder::primary_row_order` classifies the primary's once for the source; the `Arc` alone would
not have been enough, since `classify` allocates whatever the payload type is.

### The invariant, stated once

> **No per-occurrence allocation may be sized by a cap. It must be sized by that occurrence's
> own input bytes.**

That is the whole family in one line, and every shape above is a way of satisfying it: sharing
makes a repeated read cost a refcount, a view makes a merge cost nothing, and a document-scoped
memo makes a shared node cost one read. When a line inside a per-occurrence walk allocates, the
question is not whether the buffer is large but what number multiplies it — if that number is a
cap, the line is the defect however small the buffer is today, because a cap is what an input
gets to choose freely.

**The occurrence is not always an image, and the allocation is not always text.** Every shape
above is a header shape, and reading the rule as a rule about header text is how the same
family reaches the pixel path unchallenged. §10.6's `subblocks` is a second per-occurrence
walk with a cap of its own — `Subblock count`, 4096 — and it restarts the codec at every
boundary, so a *decoder state* built there multiplies exactly as a copied string does. Both
framed codecs were built per subblock, and zlib's input window was a flat 256 KiB whatever the
subblock held: 4096 subblocks of a 262 KB block allocated 1.25 GB from a 156 KB input, 8027×,
and the zstd half of the same shape 39.8 MB, 123×. Neither held anything live — the boundary
released each piece before building the next, so the *peak* was flat at 305 KB across the whole
range. **A flat peak is not evidence.** § Fuzzing's oracle counts every allocation rather than
the high-water mark, which is the measure this invariant is stated against.

### How it is enforced

`tests/header_alloc.rs` is where the rule is enforced. Its shapes assert the fuzz oracle's
bound **and** a ratio ceiling, because the bound carries a fixed 8 MiB term that hides a
multiple on any input smaller than a few megabytes: the `subblocks` instance sat at 676× its
input while passing the bound comfortably.

That file bounds the header alone — both its drivers read no pixels — so the decode path's half
of the rule is enforced beside the peak-memory shapes it belongs with, in
`tests/peak_memory.rs`. `a_subblock_costs_the_subblock_and_not_the_cap` runs for both framed
codecs and asserts the **cumulative** total rather than the peak, against the oracle's bound
and against the same block split 512 times more ways. The second assertion is the discriminating
one, for the reason the grid section below gives about one-dimensional slices: a shape at one
subblock count passes any single-point bound loose enough to hold at all, however linear in the
split the allocation behind it is.

**A fixture that exercises the code's memo-hit shape measures the memo, not the code.** Every
reader here memoizes, and byte-identical inputs are the one shape every key hits however wrongly
it is keyed — so `image.repeat(n)` builds the fixture the defect is invisible in.
`growth_over_chain_bytes_and_distinct_images` was written that way and read 17.30, 16.30, 16.30
along its image axis, perfectly flat, while a chain memo keyed on the whole image's record
sequence sat behind it at 982× the input the moment one keyword per image differed. The seed
`xisf/shared-continue-chain-across-images` was eight copies of one image for the same reason,
which seeded the fuzzer *away* from the defect it was written for.

So: **any allocation shape built from repeated elements makes them distinct, unless the test's
subject is the sharing itself.** `tests/header_alloc.rs` has `distinct_images` and
`extension_header` for the two formats, both of which put one element nothing else carries into
every repetition; the two shapes that legitimately repeat one element are
`growth_along_root_references` and `measure_shared_image`, where N references to *one* node is
the thing being measured. The same applies to a fuzz seed: a seed the mutator has to break out
of a memo to reach is a seed pointing the wrong way.

**What a shape has to cover is a reachable *path*, not a field.** For a long time every shape
in that file was one of two: N references to one image, or one root `<Metadata>` property across
N images — and both of those are covered by memos that already existed, so the file was blind to
a root non-`Metadata` node referenced by N *distinct* images, and it exercised neither a
`CONTINUE`-named record nor a chain-opening comment. Three instances lived in that gap, and the
enforcement test passed throughout.

So a shape belongs there for every path by which one element's text can be reached a number of
times bounded by a cap. Pick the reader, pick the multiplier — N references to one image, N
distinct images referencing one root node, N references to one keyword record — and write the
product, both directions verified: the shape must fail the bound *and* the ratio before the fix
and pass both after. A new reported field that can be reached through a reference still needs a
shape, but so does a new branch, a new reader, and a new way of reaching an old one.

**A list of shapes cannot cover a class, which is why the same file also states the property.**
Appending each newly-found shape does not converge — the round after the list looked complete
found two more — so the file carries a growth property beside the list: allocation must grow no faster than the
input along every multiplier axis. Two things about how it is stated are load-bearing, and both
were learnt by the property being wrong first.

- **The figure asserted is the marginal rate, `Δalloc / Δinput`, not a growth factor from the
  first point.** A growth factor has to pay for a fixed per-document cost being amortized
  differently at each size, and a tolerance generous enough to cover that is very weak over a
  wide span: 3.0 across a 16× axis admits `n^1.4` outright. The marginal rate of an honest
  affine cost is the same at every step whatever the fixed term is, so the tolerance covers only
  the variation between one mixture of added elements and another, and it can be tight — 1.5,
  against a measured worst of 1.20.

  **The base is the cheapest step on the curve, and calling the first step the cheapest corner
  is not the same thing.** The first step is the smallest increment on the axis and so the one
  a fixed cost lands on hardest: `root references to one image` read 82.19 bytes per input byte
  at step 0 and 16.69 at step 1, which made the effective tolerance on that axis 7.4× rather
  than 1.5×. A rate that reads as zero cannot be a base either — `base * slack` is then zero
  and every honest step beside it fails — so a step that allocated less than its predecessor is
  skipped rather than saturated to zero, and an axis on which no step allocated anything is a
  failure rather than a pass.

  **A curve has to stay in one regime.** Two things break that in practice, and both were found
  by the base change surfacing them: a count past `Images per source` measures the refusal
  rather than the multiplier, and a count sampled off the phase of a doubling `Vec`'s growth
  measures the staircase rather than the slope. Powers of two inside the cap satisfy both.
- **Every axis is a grid, because every real instance is a product of two multipliers.** Pin one
  and `a * b` is perfectly linear in the other, with a flat marginal rate and nothing to see:
  four one-dimensional slices passed while two gigabyte-scale instances of exactly this form sat
  in the code. What the grid asserts is that a direction's rate *does not depend on the other
  multiplier* — the bytes allocated per extra image are the same whether the thing those images
  share is four kilobytes or four hundred — which is the invariant above stated as a
  measurement. An axis added there names both of its caps.

## Autovectorization is fine; reductions are not

Element-wise autovectorization of `normalize_into` changes no bits. Rust emits no fast-math
flags, never contracts `a*b+c` into an FMA, and never reassociates. That does **not** extend
to floating-point *reductions* — a sum, a dot product, a horizontal add — where the compiler
reassociating would change the result. There are none on this path, and none may be added.

## What the guarantee assumes about its environment

Bit-exactness holds on targets with IEEE-754 `f32`/`f64` arithmetic and no excess precision:
every `x86_64-*`, `aarch64-*`, `wasm32-*`, and any 32-bit x86 target with SSE2 (which Rust's
`i686-*` targets enable by default). It does **not** hold on x87-only targets such as
`i586-*`, where excess intermediate precision breaks the `to_bits()` equality the contract
rests on.

It also assumes the **default floating-point environment** — round-to-nearest-even,
subnormals preserved. LLVM assumes both and Rust exposes no stable way to opt out, but a
host process that has set FTZ/DAZ in `MXCSR` breaks the guarantee from outside this crate,
which an embedding in a non-Rust host makes reachable. The crate cannot police that; the
assumption is stated rather than checked.

On `wasm32` a NaN result's payload and sign bit are nondeterministic by specification, so
the NaN half of the non-finite criterion compares with `is_nan()` there rather than
`to_bits()`.
