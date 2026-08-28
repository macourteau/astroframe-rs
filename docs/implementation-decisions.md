# astroframe — implementation decisions

Companion to [`design/2026-08-18-astroframe-library.md`](design/2026-08-18-astroframe-library.md),
the specification. This file does not supersede it.

It records what the design document deliberately does not: the resolutions of the choices that
document leaves to the implementer. A reviewer grading the code against the design should read
this first, so a resolved choice is not mistaken for an unresolved one.

## The four choices the design left unmade

### 1. `PropertyScope::Root` — dropped

The variant does not exist. `PropertyScope` is `Image | Metadata`.

The design doc's `properties()` rule (§ Format support matrix → *Scope is reported*) yields
exactly three kinds of entry for a selected image: that image's own child properties, root-level
`Metadata`-scope properties, and root-level properties reaching it through a `Reference` — and
that last kind is "tagged with the scope of the element it is attached *to*, not the root".
An unreferenced root-level property is not reported at all. So no path produces `Root`, and a
variant no path produces is a claim the type cannot keep.

XISF §11.4 does have three scopes; that is a fact about the format rather than about what this
crate reports, and the design doc's sentence naming three of them is describing the format.
`PropertyScope` is `#[non_exhaustive]` under the crate-wide rule in § The API, so adding `Root`
later — if a rule ever produces it — is an addition rather than a break.

### 2. `CallerSupplied { declared }` — stays `Option<String>`

The asymmetry here is real, and is forced by the design rather than chosen here.
§ The API requires `declared` to report "the file's own text verbatim whenever it declared a
`bounds` at all, **usable or not**, and `None` only when it declared none". A numeric pair
cannot represent an unparseable declaration, which is precisely the case the field exists to
preserve, so the type is `Option<String>` and its siblings stay numeric. Documented on the
field rather than smoothed over.

### 3. `chunks()` — infallible

`Reader::chunks()` returns `Chunks`, not `Result<Chunks>`. Constructing the cursor commits the
reader to the pixel phase (§ The API → *Phases, and what resets*) but reads nothing; every
error the pixel phase raises — the total-samples cap, the declared-size cross-check, the byte
caps, I/O — surfaces from `next_chunk()`, which already returns `Result`. One error path per
stream rather than two.

This changes no observable class or ordering: the pixel-phase order § Errors → Validation order
fixes is evaluated on the first `next_chunk()`, and a caller that calls `chunks()` and then
`next_chunk()` sees the same error it would have seen from a fallible constructor.

### 4. `Malformed` versus `Unsupported` for enum values — doc placement, no code choice

The rule itself is stated twice and consistently: § XISF decisions gives the criterion ("unknown
values of the enumerations decoding *depends on* — `sampleFormat`, `pixelStorage`, `colorSpace`
— are hard errors; unknown values of `imageType`, `orientation` and a `Resolution`'s `unit`
degrade to unknown and are reported as text") and § Deliberate divergences from prior art gives the class ("malformed enumerations are
`Malformed`, declined-but-valid features are `Unsupported`"). § Errors is the doc's named home
for classes and carries neither.

Nothing about the implementation is undecided, so this is recorded as a documentation finding
rather than a code choice. The implementation follows the two statements
above: an unrecognized `sampleFormat`/`pixelStorage`/`colorSpace` spelling is `Malformed`; a
recognized one this version declines (`Complex32`, `Complex64`, `CIELab`) is `Unsupported`.

## Decisions the design left to the implementer

The design's Detailed Implementation section is explicit that "choices not named here are the
implementer's". These are the ones a reviewer is most likely to want the reasoning for.

### Geometry accessors are `u32`

`Header::width`, `height` and `channels` return `Option<u32>`. § The API's rule over all four
geometry facts is that `Header` reports `None` "for any geometry or sample-format fact whose
declared value has no representable form in this crate's model", and it names "a field beyond
the representable width" among the XISF cases. `u32` is that width.

The alternative, `u64`, buys nothing a caller can use: the `Total samples` cap is 2³⁰ by
default, so any axis a decode reaches is far inside `u32`, and `select_channel(k)` and
`Image::channel(k)` both want a narrow index. A FITS `NAXISn` above `u32::MAX` reports no
geometry and declines the position as `Unsupported` — a valid, self-consistent file using an
axis length this version does not represent, which is what `Unsupported` means.

### The identity `DisplayFunction`'s literal values are reconstructed, not quoted

§11.9 gives the identity display function only as Equation [23], and the converted local copy
of the specification strips every equation to an empty image reference — so the literal
parameter values cannot be read from `reference/xisf-1.0-spec.md` at all.

`DisplayFunction::default()` uses midtones 0.5, shadows 0.0, highlights 1.0, low range 0.0,
high range 1.0 in every channel. That is the midtones transfer function's ordinary identity,
and it is what §11.9's surrounding prose states when it says a midtones balance of 0.5 defines
a linear function. Nothing in this crate applies a display function to a sample, so the values
are reported metadata rather than arithmetic; the reconstruction is recorded here so it is not
mistaken for a quotation.

### `properties()` and `keywords()` return views, not slices

`Header::properties()` returns `Properties<'_>` — a borrowed view over the root `<Metadata>`
list, the image's own properties and the single index the two interleave at — rather than
`&[Property]`. `Properties` and `PropertyIter` are exported beside `Property`.

`Header::keywords()` returns `Keywords<'_>` on the same grounds, over the image's own cards and
the list it inherits.

The design doc fixes the *report* (§ Format support matrix → *Scope is reported*, § The API)
and never fixes the return type, and the report is unchanged: the same properties, in the same
document order, each tagged with the same scope. What changes is that the merged list is never
materialized. §11.4's root properties apply to every image, so materializing it costs one copy
of the whole root list per image that adds a property of its own — 3.2 GB allocated and 2.2 GB
retained from a 1.9 MB header, at 40 000 root properties and 256 images, every count inside its
cap. That is invariant I5's unbounded-allocation clause, and no amount of sharing inside
`Property` reaches it, because what multiplies is the merge and not the text.

The view is sound because node indices are assigned in document order and a subtree is a
contiguous run of them, so an image's own properties — the ones reported at positions inside
that image — never fall *among* the root list, only between two of its entries.
`metadata::PropertySet` states the argument in full, including the nested-image case where the
two elements are not siblings.

`Properties` reads like a slice: `len`, `is_empty`, `get`, `iter`, `Index<usize>`,
`IntoIterator` and a `Debug` that prints the one merged list.

`Keywords<'_>` is the same shape over two pieces and no split index, and it exists because the
reasoning that once exempted `keywords()` was **XISF-only and false for FITS**. The exemption
read: a root-level `FITSKeyword` reaches an image only through a `Reference`, which is one of
that image's own entries, so no root list applies to every image. That holds for XISF and says
nothing about the other format. § FITS decisions mandates the opposite there — *"Both headers'
cards are always reported … the extension's followed by the primary's"* — which makes the
primary header exactly a root list applying to every image, with `FITS header cards` (4096) as
its size and `Images per source` (256) as its multiplier. Concatenating the two into an owned
`Vec` per extension allocated 53 MB and held 52 MB live from a 1.07 MB input, at 4090 `HISTORY`
cards and 256 zero-width `IMAGE` extensions.

### `RowOrder::Other` holds `Arc<str>`

`RowOrder::Other(Arc<str>)`, matching `Orientation::Other` and `ImageType::Other`, and the
`Decoder` classifies the primary header's `ROWORDER` once for the source rather than at each
image position. Both halves are needed: `classify` allocates whatever the payload type is, so
sharing the payload without sharing the classification changes nothing.

`ROWORDER` is the one card `INHERIT` gates whose **text** is reported — the other three are
lexed to numbers — so under `INHERIT = T` the primary's value is applied to every extension. Its
length is bounded by `Assembled keyword value` rather than by a card's eighty bytes, a
`CONTINUE` chain assembling it, so `Assembled keyword value` times `Images per source` is
another unbounded product: a 240 KB assembled value across 256 extensions cost 189 MB, 64 MB of
it held live, from a 1.06 MB input.

The report is unchanged — same classification, same text, `classify` keeps its signature and
still takes `&str` — and a caller matching `RowOrder::Other(text)` gets an `Arc<str>` that
derefs to `&str` where a `String` did.

The general lesson is the one `docs/intentional-patterns.md` states as the rule both the shapes
and `tests/header_alloc.rs` serve: an argument that a multiplier does not exist has to be made
per format, and this file is where it is recorded when it is.

### FITS reports `PixelStorage::Planar`

`Header::pixel_storage()` reports `Some(PixelStorage::Planar)` for FITS. § The API's rule that
"an accessor whose format does not define the concept returns `None` rather than a fabricated
value" names `orientation()` on a FITS frame and `scaling()` on an XISF one; pixel storage is
not one of those. FITS 4.0 defines the data array's ordering, so a `NAXIS = 3` cube *is* stored
plane by plane — that is a structural fact of the format rather than an attribute this crate
invented, and reporting it saves every caller a format special-case. `color_space()` is the
opposite and reports `None` for FITS, which genuinely declares no colour space.

### `Samples` derives `PartialEq`, and the bit discipline is enforced in the tests

`Samples` derives `PartialEq`, so `==` on `Samples::F32` compares eight `f32` values the way
Rust compares floats: `0.0 == -0.0` is true and `NaN == NaN` is false. That is the opposite of
this crate's own comparison rule, which is `f32::to_bits()` precisely because a sign-of-zero
difference is a real difference in a decoded frame.

The derive stays anyway, and the rule is enforced where it is violated — in the tests. Two
reasons, and the second decides it:

1. A public container of floats whose `==` means bitwise identity is surprising in a way the
   type system cannot warn about. It would report `NaN == NaN` as true, which no Rust reader
   expects from a type that is not `Eq`, and it would make two buffers a caller considers equal
   compare unequal. Removing the derive instead trades that surprise for a missing impl on a
   public type, which is a breaking API change for every downstream `assert_eq!`.
2. `to_bits()` comparison is a **test** discipline: it exists so an endpoint test cannot pass
   while the decoder moves a bit. Callers are free to compare decoded samples however their
   application means to. Enforcing it through the public API's `==` puts the rule in the wrong
   place, and — because the decoded bits are part of this crate's API surface — changes what a
   released version does for an input that already worked.

So the discipline is mechanical rather than remembered: `tests/common/mod.rs::assert_same_bits`
is the one copy every suite grades with, and the `greps` job in `.github/workflows/ci.yml`
fails the build on an `assert_eq!` over a float sample buffer anywhere under `tests/`.

### A card's value carries its kind, and a structural keyword is read only from its own kind

`Keyword` carries a `ValueKind` — `CharacterString`, `Other`, `Commentary` — beside the three
texts, and every structural keyword this crate lexes is read through a lookup that refuses a
character-string spelling. `SIMPLE = 'T'` does not assert conformance, `BITPIX = '16'` is not
the integer 16, and `NAXIS1 = '512'` sizes nothing. Each lands on the row its keyword already
has in § Errors' decline table: `SIMPLE` not being a logical value is `Malformed` at
construction, and `BITPIX`, `NAXIS`, a `NAXISn`, `PCOUNT` or `GCOUNT` not being an integer
value is `Malformed` at a declined position. The card is still **reported** verbatim — the
value text is what the file wrote, and `Keyword::value_kind()` is what says it was written in
quotes.

FITS 4.0 §4.2 gives every keyword a value *type*, so this is the standard's own distinction
rather than a strictness this crate invented. The keywords whose declared type **is** a
character string — `XTENSION`, `ROWORDER` — read from the quoted spelling as before; the rule
is the keyword's type, not a preference for unquoted text. Nothing in FITS makes the two forms
interchangeable, and reading a string as the value of a numeric keyword is the silent plausible
repair § The organizing principle refuses everywhere else.

**Leniency here protects nothing real.** The choice was measured before it was made: 302 FITS
files across two corpora — ESO, MAST (HST, JWST, TESS), IRSA, SDSS, SkyView, PixInsight,
amateur capture software, and all 120 `fpack` variants — carry 28 444 cards with quoted values
between them, and **not one** of them quotes `SIMPLE`, `BITPIX`, `NAXIS`, a `NAXISn`, `PCOUNT`,
`GCOUNT`, `GROUPS`, `EXTEND`, `BLANK`, `ZIMAGE` or `TFIELDS`.

The kind is a copyable enum rather than text, which is the `Arc<str>` discipline in
`docs/intentional-patterns.md` applied to a field that is not text at all: a `String` naming
the kind would be one allocation per card, and a header may carry 4096 cards across 256 image
positions. It rides in the padding `Arc<str>`, `usize`, `Option<usize>` and `KeywordOrigin`
leave, so `size_of::<Keyword>()` is 48 bytes with it and was 48 bytes without;
`metadata::keyword_layout::keyword_stays_packed` holds that.

`ValueKind` is public and `#[non_exhaustive]`, under the crate-wide rule in § The API. It
distinguishes only what the parse distinguishes: §4.2.1's character string is separated from
everything else because unquoting is where the fact would otherwise be lost, while §4.2.2's
logical through §4.2.6's complex floating are a parse of the reported text and are reported as
`Other` together. `Commentary` is the card with no value indicator at all, which is a third
state rather than an empty value — a `COMMENT` card and a card carrying an empty value field
are different cards.

### The five sizing keywords are read once per HDU, and the reasons stay separate

`fits::hdu::Sizing` holds `BITPIX`, `NAXIS`, the `NAXISn` run, `PCOUNT` and `GCOUNT` as the
cards wrote them, lexed once per HDU and carried on the `Hdu`. It holds **data, never a
verdict**: a value that did not read is `None`, a negative one is a negative `i64`, and the
`NAXISn` run is truncated at the first axis that did not read, that truncation point being the
fault expressed as a length. `is_image_position`, `first_fault` and `data_unit_size` each
consume it and assign their own class, in their own order, in their own words.

That split is the design's, not a convenience: § Errors → Validation order requires the decline
table to stay independent of the size formula, so that a header carrying two faults classifies
determinately. Sharing the *lexing* is what removes the drift; sharing the *judgement* would
remove the rule.

**The `999` bound on `NAXIS` goes away rather than moving.** § Errors enumerates the unsizable
reasons exhaustively — a missing, unparseable or out-of-standard-set `BITPIX`; a missing or
unparseable `NAXIS`, `NAXISn`, `PCOUNT` or `GCOUNT`; a value of one of them with no reading as
a size; or arithmetic that overflows the `u64` the computation runs in — and a parseable,
in-range-for-`i64` `NAXIS = 1000` is not among them. It needs no bound of its own either:
`NAXIS1000` is not a name an eight-byte keyword field can hold, so the axis run reaches a
`NAXISn` that is not there and returns the enumerated fault. What stays is a check that `NAXIS`
is not **negative**, which is not scope but arithmetic — the axis run over a negative `NAXIS` is
empty, so without it the unit would size as though the header declared no axes at all. Scope
belongs to `first_fault`, which already declines `NAXIS = 1` and `NAXIS > 3` as `Unsupported`.

Reading the run once has one observable consequence, and it is the two functions agreeing
rather than one of them changing its mind: a **random-groups primary missing `NAXIS1`** is
unsizable, where the size formula previously stepped over it by starting the axis product at
`NAXIS2` and never asking whether `NAXIS1` was there. `first_fault` called that same header
`Malformed` on the missing `NAXIS1` throughout, so the walk was sizing a header the decline
table had already refused. §6.1.1 makes `NAXIS1` mandatory and fixes it at 0, so a conforming
random-groups header is unaffected.

## Dependency policy: one reviewed exception

§ Dependencies of the design assumes the runtime graph is clean of the banned numeric helpers.
It is not, by exactly one entry: `typenum` matches `powi`.

```
typenum/src/int.rs:  fn powi(self, _: Z0) -> Self::Output {
```

It is type-level *integer* exponentiation on `Z0`/`PInt`/`NInt`, evaluated by the compiler. No
`f32` or `f64` appears in the crate and it computes nothing at runtime, so it is not on a decode
path. `typenum` reaches the graph through `digest` → `crypto-common` → `hybrid-array`, which is
to say only with the `checksum` feature.

This is the design's own rule working as written: a hit anywhere is a release blocker pending
review of whether that code sits on a decode path. That review is recorded here rather than in a
commit message, and the lint lane fails if `typenum` ever leaves the graph, so the exception
cannot rot into a silent hole.

## What real frames found that no fixture did

The corpus sweep is the only check that exercises the **streaming** path on real data, and that
path did not work. Every `zlib` and `zstd` block with no checksum — the one combination that
streams — stalled indefinitely; a single `next_chunk()` call never returned. Committed fixtures
did not catch it because they are small enough to fit the priming buffer, so the failure needed a
large frame to appear at all.

Three distinct defects, each found only after fixing the one in front of it:

1. **A codec handed a zero-length read concludes the stream ended.** `flate2` turns it into
   `MZFlush::Finish` and thereafter dribbles bytes; the symptom is not an error but a decode
   that crawls. Reacting to the empty read is too late. The source is read *directly* by the
   codec through a reader that borrows it for the call, so zero means end of block and nothing
   else. No buffer threshold can substitute: measured, `ruzstd` consumed a full mebibyte inside
   one call and then asked for 23 KiB more.
2. **Input exhausted is not output exhausted.** A decompressor holding bytes it could not emit
   because the destination was full must be flushed, not declared truncated.
3. **`ruzstd` retains `window_size` bytes while a frame is unfinished.** Bailing out when the
   stored block is spent strands the tail of every image whose last bytes sit in that window —
   which is why only the small-sample zstd variants failed at first, their images being small
   relative to a 2 MiB window.

Measured on a large `Float32` frame, the same bytes both ways: the streamed `zlib` path went
from never completing to one chunk per row in 408 ms, while the materialized path was unchanged
at 427 ms. The *shape* matters as much as the total — streaming reaches its first five chunks in
352 µs against 356 ms, because it does not inflate the whole block first. That difference is
what `Rows` granularity is for, and it was worth nothing while the path did not terminate.
