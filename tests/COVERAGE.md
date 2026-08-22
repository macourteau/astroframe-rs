# Invariant coverage map

Required by § Invariants of
[the design document](https://github.com/macourteau/astroframe-rs/blob/main/docs/design/2026-08-18-astroframe-library.md).

Most acceptance criteria check that a particular behaviour is right. These five are different:
they are properties the design promises to hold **everywhere**, so a single counter-example
anywhere in the crate falsifies them. Criteria not listed here are ordinary behavioural checks
and are not owned by an invariant.

Test names are `file::test_fn`. Integration tests live in this directory; `src/…` names are
unit tests in the module they pin.

## I1 — One normalization

> Every sample from every container passes through the same primitive; no format has its own
> arithmetic.

Enforced **structurally** as well as by test: `src/normalize.rs` contains no `use` of any
format module, and the `greps` CI lane fails on one. A format cannot grow its own
arithmetic without the dependency edge becoming visible in review. Layer 2 is also a pure
function of a raw sample, the scaling and the range, so the exhaustive tests need no file.

| Criterion | Tests |
| --- | --- |
| *Exhaustive `UInt16` normalization* | `normalization::exhaustive_uint16_normalization` |
| *Exhaustive `UInt8` normalization* | `normalization::exhaustive_uint8_normalization` |
| *Cross-format bit-identity* | `pipeline::cross_format_bit_identity_uncompressed`, `pipeline::cross_format_bit_identity_zlib_shuffled`, `pipeline::cross_format_bit_identity_lz4_shuffled` |

Supporting, not owned by the invariant but load-bearing for it:
`normalization::divergence_is_pinned_not_merely_avoided` (the count of divergent levels, so a
"simplification" to a division says so in plain numbers),
`normalization::endpoints_are_exact_at_every_integer_width`,
`normalization::declared_range_endpoints_are_not_generally_exact`,
`normalization::fits_unsigned_convention_matches_the_reference_two_step_form`,
`normalization::slice_normalization_equals_per_sample`.

## I2 — Delivery does not change bits

> Streamed, chunked and whole-buffer decode produce identical output.

Enforced **structurally**: tier 2 is implemented on top of tier 3, so no second whole-image
path exists to disagree with the first. The design forbids adding one.

| Criterion | Tests |
| --- | --- |
| *Streaming equals whole-buffer, bit-for-bit* | `pipeline::streaming_equals_whole_buffer_bit_for_bit` |

Supporting: `pipeline::for_each_chunk_break_stops_delivery_and_next_image_is_still_legal`,
`pipeline::every_source_mode_decodes_the_same_bits`,
`pipeline::select_channel_decodes_the_same_bits_as_slicing_a_full_decode`,
`pipeline::a_narrowed_chunk_reports_the_file_index_and_narrowed_destination_coordinates`.

## I3 — No silent transformation

> Pixels are delivered in stored order with no geometric transform and no pedestal applied;
> nothing but normalization — including its saturation step — touches a sample value.

| Criterion | Tests |
| --- | --- |
| *Report-don't-interpret is observable* | `pipeline::report_dont_interpret_is_observable_fits`, `pipeline::report_dont_interpret_is_observable_xisf` |
| *`PEDESTAL` and XISF `offset` change no pixel* | `pipeline::pedestal_changes_no_pixel`, `pipeline::xisf_offset_changes_no_pixel` |
| *Non-finite handling is total* — the clause that actually checks the saturation step | `normalization::non_finite_handling_is_total` |

Supporting: `fits_decisions::blank_is_reported_and_no_sample_is_substituted` (a `BLANK` integer
is reported and never substituted), `fits_decisions::roworder_spellings_normalize_and_unknown_values_survive_verbatim`.

## I4 — No allocation from an *unvalidated* declared size

> Every pixel buffer is sized from validated geometry, and declared sizes are cross-checks
> only. The one buffer that grows toward a declared length — the XML header — grows
> incrementally under a cap, never pre-sized.

| Criterion | Tests |
| --- | --- |
| *Adversarial suite* | `adversarial::compressed_usize_disagrees_with_geometry`, `adversarial::uncompressed_block_size_mismatch`, `adversarial::sample_block_byte_cap_exceeded` — the cases where a declared size is used as a cross-check instead of an allocation size — and the rest of the suite |
| *Every cap has a test that trips it* | `fits_caps::*`, `xml_guards::the_stored_block_cap_trips_on_a_sequential_source`, `xml_guards::the_subblock_count_cap_trips`, `xml_guards::the_zstd_declared_window_cap_trips`, `xml_guards::a_declared_header_length_above_the_cap_is_rejected_before_the_read` |
| *A decompression bomb is refused without materializing* | `bombs::a_decompression_bomb_is_refused_without_materializing` (zlib, LZ4 and the zstd declared window, with the allocation asserted) |
| *Fuzzing* | `fuzz_replay::synthetic_seeds_replay_without_panicking`, `fuzz_replay::committed_corpus_and_crash_artifacts_replay`, and the six `fuzz/fuzz_targets/*` under `cargo fuzz` |

The stored-block cap closes I4's one remaining hole — a declared `attachment:pos:size` that
becomes an allocation — so it is deliberately tested on a **sequential** source, the shape with
no file length to fall back on.

> **On the names in this file.** Most entries read `file::name` and select with
> `cargo test name`. Some do not: `tests/header_alloc.rs`, `tests/bombs.rs` and
> `tests/peak_memory.rs` install a `#[global_allocator]` and measure the whole process, so each
> holds **one** `#[test]` that calls its shapes as plain functions — the harness runs tests in
> parallel, and two of these racing on one counter measure each other. An entry naming such a
> shape names a function, not a selectable test; run its file's single test instead.

## I5 — Malformed input errors, never aborts

> No input, however hostile, causes a panic, a hang, or unbounded allocation.

| Criterion | Tests |
| --- | --- |
| *Adversarial suite* | `adversarial::*`, each asserting the error class **and** that no value is returned alongside it |
| *Caller misuse is an error, not a panic* | `fits_declines::a_wrong_sized_destination_is_invalid_request`, `fits_declines::select_channel_beyond_the_channel_count_is_invalid_request`, `fits_declines::select_channel_where_the_channel_count_is_none_is_invalid_request`, `fits_declines::with_bounds_after_the_pixel_phase_is_invalid_request`, `fits_declines::read_samples_into_with_a_mismatched_variant_is_invalid_request`, `fits_declines::a_second_with_bounds_and_a_second_select_channel_are_last_wins`, `fits_declines::an_invalid_second_with_bounds_leaves_the_first_range_in_force` |
| *The XML guards* | `xml_guards::*`, including `quick_xml_resolves_only_the_five_predefined_entities_and_does_not_recurse`, which pins a property of the *dependency* rather than of this crate |
| *Fuzzing* | as I4 |
| *the unbounded-allocation clause*, held per-shape rather than only by the fuzzer | `header_alloc::header_parsing_stays_within_the_fuzz_oracles_allocation_bound` — every multiplying header shape and six two-multiplier growth grids, each asserting the fuzz oracle's own bound **and** a per-shape ratio, because the bound's fixed 8 MiB term admits a hundredfold multiple at the input sizes these shapes reach |

The **hang** clause is the one a fuzzer covers worst: a libFuzzer run reports a hang only as a
timeout, and fuzz inputs are finite by construction, so an unbounded skip over a pipe is held by
the HDU-traversal cap and its test (`fits_caps::hdus_per_advance_cap_trips`) rather than by the
fuzzer. `src/fits/cards.rs`'s `hostile_lengths_error_rather_than_panic` sweeps every prefix
length of a header region for the same reason.

This section used to claim the hang clause was one **no test can reach**. That was wrong, and
believing it is part of why the defect survived: a `zlib` stream ending before its declared size
hung on an 80-byte file, and
`adversarial::zlib_stream_ending_before_the_geometry_implied_size` reaches it in microseconds.
A hang is reachable by a test exactly when some loop can fail to make progress — which is a
property to assert about the loop, not a hazard to characterize as untestable.

## Where the evidence is weakest

Recorded rather than left to be discovered:

- **A reported granularity is worth only what measures it.** Two rows of § Streaming's table
  were reported correctly while the decode did not honour them: a subblocked `zlib` block
  reported `Rows` and did not decode at all, and `lz4` + `subblocks` reported `Block` while
  materializing the whole block. The first was caught by asserting *pixels* beside the
  granularity (`xisf_decisions::a_subblocked_zlib_block_streams_by_rows`), the second only by
  asserting *peak memory* beside it (`peak_memory::peak_decode_memory_meets_the_stated_target`)
  — pixels alone cannot tell the two apart, because both paths produce the same bytes.
- **The allocation clause is held as *bounded*, not as *proportional*, and one shape shows the
  difference.** `<Reference>` elements can compose a distinct `CONTINUE` chain for each of 256
  images, so every image assembles a genuinely different value and no sharing applies. That
  allocates about 1.05 GB from a 2.2 MB input — inside `Images per source × Assembled keyword
  value`, so I5 holds, and far outside the fuzz oracle's proportionality bound, which is a
  stronger property this design does not promise.
  `header_alloc::composed_chains_across_distinct_images` pins it against an **absolute**
  ceiling and asserts that it *exceeds* the oracle's bound, so the record cannot go stale
  silently. § Fuzzing derives the figure and records why raising `ALLOC_MULTIPLE` to admit it
  would switch the oracle off for the repeated-assembly class: the two allocate the same
  amount and their ratios differ by at most a factor of two.

- **I5's hang clause was reachable, and a test *can* reach it.** This section previously said
  "the hang clause is the one no test can reach". A `zlib` stream that ends before the
  geometry-implied size, with stored bytes still unread behind it, spun forever: the streaming
  loop's only escape for a non-productive iteration required the *input* to be exhausted, and
  `StreamEnd` with bytes still buffered satisfies neither that nor the loop's exit condition.
  Eight compressed bytes and ten of filler are enough, on the commonest streamed codec, under
  default limits. `adversarial::zlib_stream_ending_before_the_geometry_implied_size` now covers
  it, and the general lesson is in the loop's own comment: **every iteration must either make
  progress or return**, which is a property to assert about a loop rather than a consequence to
  hope for from its exit conditions.
- **A cap the caller may raise is not a bound the code may rely on.** `Geometry::total_samples`
  saturates, so the total-samples comparison cannot be fooled — but a caller raising that cap
  to `u64::MAX` (which § The caps contemplates, and which three tests here do) let the
  saturated count flow into products that size a staging row and a destination. Those panicked
  under overflow checks and wrapped without them. The gate in `check_total_samples` now also
  requires the geometry's *byte* extent to be computable, which removes the class early —
  though not every downstream site, since two add a *file offset* on top of a geometry product
  and an offset is no factor of the geometry.
  `adversarial::a_geometry_whose_byte_extent_exceeds_u64_under_a_raised_cap` and its FITS twin
  grade **that gate** on both entry points — and this entry used to claim they graded the two
  offset sites as well, which they do not: each picks a geometry extreme enough to trip the
  gate itself, so neither ever reaches the line it was cited for. A coverage claim that names
  the wrong test is worse than a gap, because it stops anyone looking. The two offset sites are
  graded by tests of their own, one per format, and they divide on a rule worth stating: a
  **declared** offset needs its own arithmetic checked, an **observed** one is bounded by the
  bytes that produced it.
  `adversarial::a_row_offset_past_u64_is_refused_rather_than_computed` reaches the XISF site,
  where `position` comes out of a `location` attribute and a bare `+` overflowed;
  `fits_caps::a_row_offset_past_u64_is_refused_rather_than_computed` asserts that the FITS
  site, whose `data_start` is observed, is covered by the byte-extent gate rather than needing
  a check.
- **I5 was falsified by a header three integers wide, and the fuzzers could not reach it.**
  `Geometry::total_samples` multiplied three `u32` axes in `u64`, which holds only 2^64 of the
  2^96 they reach — panicking wherever overflow checks are on and wrapping elsewhere, where
  the wrap walks a hostile declaration straight through the cap meant to reject it. The
  function's own doc comment asserted the opposite ("in `u64` so a hostile declaration cannot
  overflow it"), and `fuzz/src/lib.rs` cited that comment as the reason its own oracle was
  safe. Two lessons: a comment claiming an invariant is not evidence of it, and a fuzzer is
  weak precisely where the hostile value is a *specific* large constant — `4294967295` three
  times is trivially typed and essentially unreachable by random mutation. The adversarial
  suite had cases either side of the gap (one axis overflowing `u32` at parse, a product of
  10^13 well inside `u64`) and none in the multiplicative middle.
- **The XISF path had no independent oracle for a long time, and now has the strongest one
  in the project.** Its correctness rested on the exhaustive normalization tests plus cross-format
  identity over *synthetic* fixtures — every premise this crate's own. The corpus's XISF
  variants were written by PixInsight *from* the FITS frames they came from, so
  `corpus::xisf_variants_decode_to_the_same_bits_as_the_fits_they_came_from` compares the two
  decodes over **1080 files and 41.8 billion pixels**, with the FITS side itself bit-verified
  against `fitsrs`. Three of the five sample formats come out **bit-exact**. The lesson is that
  the oracle was sitting in the corpus the whole time, unused, because nobody had noticed that
  two directories held the same pixels.
- **Derive a tolerance, then check it — never fit one.** The `uint8` bound was written `1/512`
  ("half of 256 levels") and rejected a corpus that was correct: a `UInt8` level normalizes as
  `L/255`, so half a step is `1/510`, and the measured worst falls between the two. A tolerance
  fitted to the measurement would have passed and hidden a misunderstanding of the denominator
  the crate's central contract is about.
- **The memo was per-image for a long time, while the rule said "read once".** Sharing a value
  reached through a `<Reference>` was implemented inside each reader, so "read once and shared"
  meant *once per image*: N distinct images referencing one root node still made N copies, and
  a `CONTINUE`-named record bypassed the memo entirely by being handled upstream of it. The
  cache is document-scoped now. The general lesson is that a memo's **key scope** is part of
  the claim a memo makes, and "read once" without saying once-per-what is not a claim at all.
- **One member of the class is not a copy, and no amount of sharing closes it.** A `CONTINUE`
  chain reached through many references assembles a value that genuinely is that long — a
  gigabyte from a 553 KB header. That one needed a cap (`keyword_value_bytes`), and it is why
  the class took one instance longer to close than it looked: every earlier instance was a
  copy, so this one was read as another until it was measured.
- **A green fuzz lane is not evidence the oracle holds.** The first real scheduled run
  executed about 52 million cases across all six targets at `-max_len=4194304` and found
  nothing. In the same period, hand-built inputs found four separate oracle-falsifying shapes,
  the worst allocating 3.11 GB from a one-megabyte header. The shapes need structure a mutator
  reaches slowly — a `uid` and a matching `ref`, then thousands of references to it — so
  coverage-guided fuzzing is close to blind here however long it runs. Directed inputs derived
  from *reading the code* are what found every one of them, and the fuzz corpus should carry
  them as seeds rather than waiting for the fuzzer to rediscover them.
- **The fuzz targets cannot find a decompression bomb.** They run with tightened caps, so a
  bomb never grows large enough to matter there; the zlib bomb that decoded clean was found by
  the ported adversarial corpus, not by 28 million fuzz executions.
- **The corpus tier is opt-in and never runs in CI.** `corpus::*` and
  `differential::corpus_native_samples_match_fitsrs` are `#[ignore]`d and read a directory
  named by an environment variable, because the corpus carries observatory coordinates at full
  precision and is never committed. They have run green across the whole corpus, but only on a
  maintainer's machine and only when invoked by hand — so a regression there is caught by
  whoever remembers to run it, not by a push.
