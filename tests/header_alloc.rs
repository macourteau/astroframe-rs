//! *Header parsing allocates within the fuzz oracle's own bound* — the header half of the
//! allocation criterion, on both formats.
//!
//! `tests/peak_memory.rs` bounds what a **decode** allocates on top of its destination. This
//! file bounds what reading a header allocates before a single pixel is read, on the shapes
//! that make that cost visible: an attribute-heavy header and an element-heavy one to start
//! with. Both sit well inside the shipped 8 MiB `xml_header_bytes` cap, so neither is refused,
//! and both are the shapes the fuzzer reaches by growing a header rather than an image.
//!
//! Most of the file is XISF, that being where the multiplying readers are, but it is **not an
//! XISF file**: a FITS extension reports the primary's whole card list, which is a root list
//! with a per-image multiplier exactly as root `<Metadata>` is, and a gigabyte-scale instance of
//! it went unmeasured here for want of one FITS fixture.
//!
//! The bound is **the fuzz oracle's**, restated here so the two cannot drift: a header shape
//! that fails here fails `fuzz/`'s `walk_headers` oracle for the same reason, and § Fuzzing's
//! rule that "a failure at the current `ALLOC_MULTIPLE` is presumed to be a bug in the allocation, not
//! a bad bound" applies to this file exactly as it applies there.
//!
//! **Both measurements live in one `#[test]`, deliberately** — the same reason
//! `tests/peak_memory.rs` gives: a `#[global_allocator]` counts the whole process, and the
//! harness runs `#[test]`s on parallel threads, so a second test building its fixture lands in
//! the counter this one just reset.

#[path = "common/alloc.rs"]
mod alloc;
mod common;

use std::io::Cursor;

use astroframe::{KeywordOrigin, Reader, RowOrder};

use common::Hdu;
use common::xisf::Unit;

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// The fuzz oracle's general allocation bound, `fuzz/src/lib.rs`'s `bound()`:
/// `ALLOC_MULTIPLE × input_length + xml_header_bytes`, at the `ALLOC_MULTIPLE` § Fuzzing fixes and the shipped
/// 8 MiB header cap the fuzz `limits()` leaves untouched.
///
/// Stated as the formula rather than as a measured figure with slack, so that raising it here
/// is as visible as raising it there — and so a shape that passes here cannot fail the oracle.
fn oracle_bound(input_len: usize) -> usize {
    // Kept in step with `fuzz/src/lib.rs`'s `ALLOC_MULTIPLE` and `bound()` by hand, the fuzz
    // crate not being a dependency of the test crate. The multiple is 32 rather than 8 because
    // 8 was derived from the pixel path alone; § Fuzzing records why and what it cost to
    // establish that the difference is structural rather than a defect.
    32 * input_len + (8 << 20)
}

/// Both header shapes, measured one after the other in a single test.
#[test]
fn header_parsing_stays_within_the_fuzz_oracles_allocation_bound() {
    attribute_heavy_header();
    element_heavy_header();
    keywords_under_an_image_are_materialized_twice();
    the_smallest_element_is_the_worst_shape();
    a_large_attribute_on_a_shared_image();
    one_keyword_reached_through_many_references();
    root_metadata_shared_across_many_images();
    one_keyword_heavy_image_reached_through_many_references();
    one_property_heavy_image_reached_through_many_references();
    one_large_property_reached_through_many_in_image_references();
    an_unrecognized_property_type_reached_through_many_in_image_references();
    large_attributes_on_a_shared_image();

    // The property, not the list. See the section at the end of this file.
    growth_along_root_references();
    growth_along_distinct_images();
    growth_along_a_continue_chain();
    growth_along_metadata_with_own_properties();
    root_metadata_when_every_image_adds_a_property_of_its_own();
    root_metadata_property_count_across_many_images();
    a_full_subblocks_list_on_a_shared_image();
    an_orphaned_continue_record_reached_through_many_references();
    a_chain_opening_keywords_comment_reached_through_many_references();
    one_root_element_referenced_by_many_distinct_images();
    one_continue_chain_assembled_by_many_distinct_images();
    a_shared_chain_says_what_each_image_actually_carries();
    composed_chains_across_distinct_images();
    a_full_primary_header_reported_by_many_extensions();
    a_long_inherited_row_order_applied_by_many_extensions();

    // The two-multiplier grids. See the section at the end of this file.
    growth_over_chain_bytes_and_distinct_images();
    growth_over_primary_cards_and_image_extensions();
    growth_over_inherited_row_order_and_image_extensions();
}

/// `count` `<Image>` elements, each carrying `children` and **one keyword no other image
/// carries**.
///
/// The distinguishing keyword is not decoration, and `image.repeat(count)` is not a cheaper
/// spelling of this. Every reader in this crate memoizes, and byte-identical images are the
/// one shape every memo key hits however wrongly it is keyed: a fixture built by repetition
/// measures the memo rather than the code. `growth_over_chain_bytes_and_distinct_images` was
/// built that way for a round and read 17.30, 16.30, 16.30 along its image axis — perfectly
/// flat — while a chain memo keyed on the whole image's record sequence sat behind it. One
/// `<FITSKeyword>` per image is outside that key and defeats it: the same grid's corner went
/// to 982x its input, and the fuzz oracle's own bound with it.
///
/// So every shape here that multiplies an image count builds distinct images, and the two
/// that do not — `growth_along_root_references` and `measure_shared_image` — are the two whose
/// subject *is* the sharing.
fn distinct_images(count: usize, children: &str) -> String {
    (0..count)
        .map(|i| {
            format!(
                r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data><FITSKeyword name="U{i:05}" value="1" comment=""/>{children}</Image>"#
            )
        })
        .collect()
}

/// The multiplication shapes, measured against a **ratio** as well as the oracle's bound.
///
/// `oracle_bound` carries a fixed 8 MiB term, and every shape below is built from around a
/// megabyte of input, so the bound alone admits a shape allocating a hundred times its input —
/// which is how a 32.3× shape once passed a test meant to pin 26×. What these shapes are about
/// is the *multiple*: the same header text reached through N references cost N copies, so the
/// figure that must not move is bytes per input byte, and that is asserted here explicitly.
///
/// The ceilings are the measured ratio with roughly a quarter's headroom, and none of them may
/// exceed `ALLOC_MULTIPLE` by more than the one case that documents why.
fn measure_multiplied(shape: &str, unit: &[u8], ratio_ceiling: f64) {
    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.to_vec())).expect("the unit constructs");
    let (total, peak) = (alloc::total(), alloc::peak());
    drop(reader);

    let bound = oracle_bound(unit.len());
    let ratio = total as f64 / unit.len() as f64;
    println!(
        "{shape}: input {} total_alloc {total} peak {peak} bound {bound} ratio {ratio:.2}x \
         (ceiling {ratio_ceiling:.0}x)",
        unit.len()
    );
    assert!(
        total <= bound,
        "{shape}: cost {total} bytes against a {bound}-byte bound"
    );
    assert!(
        ratio <= ratio_ceiling,
        "{shape}: allocated {ratio:.2}x its {} input bytes, above the {ratio_ceiling:.0}x \
         ceiling. The bound above still passes — it carries a fixed 8 MiB term — so this is \
         the assertion that says header text is being multiplied again. Name the buffer that \
         grew with the reference count before touching the ceiling.",
        unit.len()
    );
}

/// **One root-level `<Property>` reached through 4096 in-image `<Reference>` elements.**
///
/// §11.13 lets an image reference a root-level property, and § XISF decisions requires each
/// reference to be reported at its own position — so N references are N `Property` values.
/// Reading the element once per reference copied its texts every time: a 48 KB `value` — well
/// inside the 1 MiB `Attribute value length` cap — over 4096 references allocated **200 MB and
/// retained 199 MB from a 130 KB header**, 1539× its input. That is invariant I5's
/// unbounded-allocation clause at tier 1.
///
/// The element is read once and the built `Property` shared: its texts are `Arc<str>`, so a
/// repeat costs a refcount. What remains is the 4096 `Property` values the report itself is,
/// which is the file's own content and not a copy of it.
fn one_large_property_reached_through_many_in_image_references() {
    measure_multiplied(
        "4k in-image references to one 48 KB property",
        &property_reached_by_references(&format!(
            r#"type="String" value="{}""#,
            "v".repeat(48_000)
        )),
        32.0,
    );
}

/// The same shape with the weight in the **`type`** attribute rather than the value.
///
/// `PropertyType::Other` carries an unrecognized type name verbatim, and §11.1.1 puts no length
/// on it, so it multiplies exactly as the value does — 1539× before, and it is a separate
/// assertion because it is a separate field: sharing the value alone would leave this at 1539×.
fn an_unrecognized_property_type_reached_through_many_in_image_references() {
    measure_multiplied(
        "4k in-image references to a 48 KB property type",
        &property_reached_by_references(&format!(r#"type="{}" value="v""#, "T".repeat(48_000))),
        32.0,
    );
}

/// One root-level `<Property uid="p">` carrying `attrs`, reached by 4096 in-image references.
fn property_reached_by_references(attrs: &str) -> Vec<u8> {
    let refs = r#"<Reference ref="p"/>"#.repeat(4096);
    Unit::new()
        .xml(&format!(
            r#"<Property uid="p" id="P" {attrs}/><Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>{refs}</Image>"#
        ))
        .build()
}

/// **Every remaining `<Image>` attribute that carries text, at the `Attribute value length`
/// cap, on an image reached by 256 root-level `<Reference>` elements.**
///
/// `a_large_attribute_on_a_shared_image` shared `id`, `uuid` and the mosaic pattern; these are
/// the four that were left owned, and each retained about 276 MB from a one-megabyte header —
/// 263× its input, and 1.11 GB for the four together.
///
/// `sampleFormat` is the odd one and the worst: an unrecognized value is not stored as a value
/// at all but interpolated into a [`DeclineReason`]'s message, so the megabyte comes back as a
/// megabyte-long sentence copied per occurrence. `DeclineReason::reason` is shared for that
/// reason while `Error` keeps its `String`: the error is built when a pixel call is actually
/// made on the declined position, which happens once.
fn large_attributes_on_a_shared_image() {
    let big = "x".repeat(1 << 20);
    let valid = r#"geometry="1:1:1" sampleFormat="UInt16""#;
    for (shape, attrs, ceiling) in [
        (
            "1 MiB bounds x 256 references",
            format!(r#"{valid} bounds="{big}""#),
            12.0,
        ),
        (
            "1 MiB orientation x 256 references",
            format!(r#"{valid} orientation="{big}""#),
            12.0,
        ),
        (
            "1 MiB imageType x 256 references",
            format!(r#"{valid} imageType="{big}""#),
            12.0,
        ),
        (
            "1 MiB unreadable sampleFormat x 256 references",
            format!(r#"geometry="1:1:1" sampleFormat="{big}""#),
            16.0,
        ),
        (
            "four 1 MiB attributes x 256 references",
            format!(
                r#"geometry="1:1:1" sampleFormat="{big}" bounds="{big}" orientation="{big}" imageType="{big}""#
            ),
            12.0,
        ),
    ] {
        let refs = r#"<Reference ref="im"/>"#.repeat(256);
        let unit = Unit::new()
            .xml(&format!(
                r#"<Image uid="im" {attrs} location="embedded"><Data encoding="base64">AAA=</Data></Image>{refs}"#
            ))
            .build();
        measure_multiplied(shape, &unit, ceiling);
    }
}

/// **Root `<Metadata>` shared across 256 images that each add a property of their own.**
///
/// `root_metadata_shared_across_many_images` shares the built list only when an image adds
/// nothing; an image with one property of its own takes the merge path, which copies every root
/// property into that image's list. One 40-byte `<Property>` per image was enough to restore the
/// whole 262 MB — the sharing was conditional on a shape the pathological input simply does not
/// have.
///
/// The text is not what multiplies on this path: a `Property` is five `Arc`s and copying one is
/// five refcounts, so one megabyte of root property text is one copy of that text however many
/// images take the merge. What multiplies is the *count*, which the shape below is for.
fn root_metadata_when_every_image_adds_a_property_of_its_own() {
    let value = "p".repeat(1_000_000);
    let images = distinct_images(256, r#"<Property id="Q" type="String" value="q"/>"#);
    let unit = Unit::new()
        .xml(&format!(
            r#"<Metadata><Property id="X" type="String" value="{value}"/></Metadata>{images}"#
        ))
        .build();
    measure_multiplied(
        "1 MB root metadata x 256 images each with their own property",
        &unit,
        10.0,
    );
}

/// **Forty thousand root `<Metadata>` properties beside 256 images that each add one of their
/// own.**
///
/// The shape above with the weight moved from one property's text to the property *count*,
/// which is the axis nothing bounded: the merge was one copy of the whole root list per image,
/// and `XML element count` times `Images per source` is a product no part of the input relates
/// to. It allocated 3.2 GB and retained 2.2 GB from a 1.9 MB header — 1686x its input, past
/// `oracle_bound` by a factor of 46 — while the megabyte-shaped twin above sat at 6.5x
/// throughout, which is why that one is not the guard for this one.
///
/// The merged list is not built: `Header::properties` reports the shared root list, the image's
/// own and the one position they interleave at, as a view over the three.
fn root_metadata_property_count_across_many_images() {
    let metadata: String = (0..40_000)
        .map(|i| format!(r#"<Property id="M{i}" type="String" value="m"/>"#))
        .collect();
    let images = distinct_images(
        256,
        r#"<Property id="own" type="String" value="oooooooooooooooooooo"/>"#,
    );
    let unit = Unit::new()
        .xml(&format!("<Metadata>{metadata}</Metadata>{images}"))
        .build();
    measure_multiplied(
        "40k root metadata properties x 256 images each with their own",
        &unit,
        21.0,
    );
}

/// **A `subblocks` list at the `Subblock count` cap, on an image reached by 256 references.**
///
/// The same shape one layer down: the list is not header *text* but it is derived from one
/// attribute, and it was owned by the `BlockPlan` each occurrence carries — 4096 pairs is 64 KB,
/// and 256 occurrences made 17 MB from a 26 KB header, 676× its input.
///
/// This one stayed inside `oracle_bound` throughout, the fixed 8 MiB term absorbing it, which is
/// exactly why the ratio is asserted: by the bound alone nothing here ever looked wrong.
///
/// Its ceiling is the loosest of the family and above `ALLOC_MULTIPLE`, because the input is
/// 26 KB and the fixed cost of parsing 4096 subblock pairs is charged against it: the same
/// fixture at **one** occurrence measures 20.4×, before any multiplication at all. What the
/// ceiling pins is the slope, not the intercept.
fn a_full_subblocks_list_on_a_shared_image() {
    let list = vec!["1,1"; 4096].join(":");
    let refs = r#"<Reference ref="im"/>"#.repeat(256);
    let unit = Unit::new()
        .attached(
            &format!(
                r#"<Image uid="im" geometry="4096:1:1" sampleFormat="UInt8" compression="zlib:4096" subblocks="{list}" {{loc}}/>"#
            ),
            vec![0u8; 4096],
        )
        .xml(&refs)
        .build();
    measure_multiplied("4096 subblocks x 256 references", &unit, 40.0);
}

/// **One root `<FITSKeyword name="CONTINUE">` reached through 2048 in-image references.**
///
/// A `CONTINUE`-named record is folded before the keyword memo is consulted, so every reference
/// took the orphan branch and built the commentary keyword afresh: `join_body` rejoins the
/// `value` and `comment` attributes — a mebibyte each at the `Attribute value length` cap — and
/// `Keyword::new` packs a second copy of the result, both of them **retained** in the reported
/// list. A one-megabyte header allocated **6.45 GB and retained 2.16 GB**, 5918× its input, and
/// at the shipped caps the same shape reaches about 105 GB.
///
/// It is the branch the rule's memo did not cover rather than a field the rule did not name,
/// which is why the entry in `docs/intentional-patterns.md` now speaks of text *built* per
/// occurrence and not only of text *held* per occurrence.
fn an_orphaned_continue_record_reached_through_many_references() {
    let big = "v".repeat(1 << 20);
    measure_multiplied(
        "1 MiB orphaned CONTINUE x 2048 references",
        &keyword_reached_by_references(&format!(r#"name="CONTINUE" value="{big}" comment="""#)),
        12.0,
    );
}

/// **One chain-opening `<FITSKeyword>` with a large comment, reached through 2048 references.**
///
/// A value ending in `&` is the only condition for opening a `CONTINUE` chain, so a keyword
/// reached through N references opens N chains — and opening one copied the `comment`
/// attribute, which the cap lets be a mebibyte. `Chain::value` was made lazy for exactly this
/// shape and `Chain::comment` was left eager beside it: a 256 KB comment over 2048 references
/// allocated **540 MB from a 300 KB header**, 1779×, against 10× for the same fixture without
/// the trailing `&`. Nothing a `Chain` carries is copied out of the document: the whole value
/// is built once, when the chain closes, by whichever image assembles it first.
///
/// The control is measured too, because it is what says the cost is the chain and not the
/// comment: the two fixtures differ by one byte.
fn a_chain_opening_keywords_comment_reached_through_many_references() {
    let big = "c".repeat(256 << 10);
    for (shape, value, ceiling) in [
        (
            "256 KB comment on a chain-opening keyword",
            "'v&amp;'",
            13.0,
        ),
        ("the same keyword without the trailing &", "'v'", 13.0),
    ] {
        measure_multiplied(
            &format!("{shape} x 2048 references"),
            &keyword_reached_by_references(&format!(r#"name="K" value="{value}" comment="{big}""#)),
            ceiling,
        );
    }
}

/// One root-level `<FITSKeyword uid="k">` carrying `attrs`, reached by 2048 in-image references.
fn keyword_reached_by_references(attrs: &str) -> Vec<u8> {
    let refs = r#"<Reference ref="k"/>"#.repeat(2048);
    Unit::new()
        .xml(&format!(
            r#"<FITSKeyword uid="k" {attrs}/><Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>{refs}</Image>"#
        ))
        .build()
}

/// **One root element referenced by 256 *distinct* `<Image>` elements**, for every reader a
/// per-call memo could not cover.
///
/// This is the shape the rule's safeguard was blind to. Every other case here is either N
/// references to *one* image — which the occurrence memo covers — or root `<Metadata>`, which
/// the walk builds once. A memo built inside `collect_children` or the keyword fold is rebuilt
/// on every call, and a call is one image, so N distinct images each referencing one root node
/// read that node N times: 275 MB for a `<ColorFilterArray>` pattern at the `Attribute value length`
/// cap, 812 MB for a `<FITSKeyword>`, 276 MB for a `<Property>`, each from a one-megabyte
/// header. `read_cfa` had no memo of any kind.
///
/// The `<DisplayFunction>` case is the one that holds no header text at all, and it is here
/// because the rule is about text *built* per occurrence: reading one splits five attributes
/// into `&str` vectors, and half a million fields is an eight-megabyte vector per read —
/// **2.15 GB allocated against a 14 MB peak**, so nothing that watched retention would ever
/// have seen it.
///
/// The cache is document-scoped for that reason, which is what "read once and shared" was always
/// supposed to mean.
fn one_root_element_referenced_by_many_distinct_images() {
    let big = "x".repeat(1 << 20);
    // Half a mebibyte of two-character fields, which is what makes the split expensive.
    let fields = "0:".repeat((1 << 19) - 8);
    for (shape, root, ceiling) in [
        (
            "1 MiB root CFA pattern x 256 distinct images",
            format!(r#"<ColorFilterArray uid="r" pattern="{big}" width="2" height="2"/>"#),
            10.0,
        ),
        (
            "1 MiB root FITSKeyword x 256 distinct images",
            format!(r#"<FITSKeyword uid="r" name="N" value="{big}" comment=""/>"#),
            12.0,
        ),
        (
            "1 MiB root Property x 256 distinct images",
            format!(r#"<Property uid="r" id="P" type="String" value="{big}"/>"#),
            10.0,
        ),
        (
            "1 MiB root DisplayFunction x 256 distinct images",
            format!(r#"<DisplayFunction uid="r" m="{fields}"/>"#),
            18.0,
        ),
    ] {
        let images = distinct_images(256, r#"<Reference ref="r"/>"#);
        let unit = Unit::new().xml(&format!("{root}{images}")).build();
        measure_multiplied(shape, &unit, ceiling);
    }
}

/// **One keyword-heavy `<Image uid>` reached through 256 root-level `<Reference>` elements.**
///
/// This is the spelling §11.13's second worked example uses, so it is a conforming shape rather
/// than a hostile one, and § XISF decisions requires one reported occurrence per reference.
/// Building each occurrence from the node rebuilt that image's whole keyword list every time: a
/// 3.9 MB header — inside every shipped cap — allocated **4.60 GB and retained 2.15 GB**, which
/// is process death at tier 1 on the header-only path a consumer runs over an untrusted
/// size-capped prefix. That is invariant I5's unbounded-allocation clause.
///
/// The per-image work is memoized on the image's node index instead, so the occurrences share
/// the built `Header` — its keyword and property lists are `Arc`s — while each stays its own
/// occurrence.
fn one_keyword_heavy_image_reached_through_many_references() {
    let mut children = String::new();
    for i in 0..80_000u32 {
        children.push_str(&format!(
            r#"<FITSKeyword name="K{i:05}" value="1" comment=""/>"#
        ));
    }
    measure_shared_image("80k FITSKeyword x 256 references", &children, 60_000_000);
}

/// **One property-heavy `<Image uid>` reached through 256 root-level `<Reference>` elements.**
///
/// The property twin of the case above, and the more expensive one per input byte: the merged
/// property list is rebuilt, sorted and collected per occurrence. A 2.1 MB header allocated
/// **5.23 GB and retained 1.55 GB**.
fn one_property_heavy_image_reached_through_many_references() {
    let mut children = String::new();
    for i in 0..40_000u32 {
        children.push_str(&format!(
            r#"<Property id="P{i:05}" type="String" value="v{i:05}"/>"#
        ));
    }
    measure_shared_image("40k Property x 256 references", &children, 60_000_000);
}

/// One `<Image uid="img">` carrying `children`, plus 256 bare root-level references to it.
///
/// Both the total and the peak are asserted: the total is what the fuzz oracle measures, and
/// the peak is what "retains 1.6 GB" meant — a shape can stay inside a cumulative bound while
/// still holding every copy at once, so the retention needs its own assertion.
fn measure_shared_image(shape: &str, children: &str, peak_bound: usize) {
    let refs = r#"<Reference ref="img"/>"#.repeat(256);
    let unit = Unit::new()
        .xml(&format!(
            r#"<Image uid="img" geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>{children}</Image>{refs}"#
        ))
        .build();

    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let (total, peak) = (alloc::total(), alloc::peak());
    drop(reader);
    let bound = oracle_bound(unit.len());
    println!(
        "{shape}: input {} total_alloc {total} peak {peak} bound {bound}",
        unit.len()
    );
    assert!(
        peak < peak_bound,
        "{shape}: 256 references to one image retain {peak} bytes, above the {peak_bound}-byte \
         bound; they retained 1.6 GB before the per-image work was shared"
    );
    assert!(
        total <= bound,
        "{shape}: 256 references to one image cost {total} bytes against a {bound}-byte bound"
    );
}

/// **The worst legal shape is the *smallest* element**, not the most attribute-laden one.
///
/// Per-node overhead is fixed and the source text it is charged against shrinks to four bytes,
/// so `<A/>` costs about 26× its own length where a `<FITSKeyword>` costs 9×. § Fuzzing's
/// derivation of `ALLOC_MULTIPLE = 32` rests on that 26×, and per byte the headroom over it is
/// only 1.23×. An earlier derivation named a 17.7× shape and called it 25×, which is how the
/// number came to be set against the wrong exemplar — hence this test.
///
/// The ratio does not bind in practice, and this measures why: `XML element count` caps the
/// element count at 100 000, so the worst *reachable* header of this shape is about 400 kB of
/// input, where the bound's fixed 8 MiB term still dominates. Per-byte headroom of 1.23× and
/// reachable headroom of roughly 2× are different numbers, and it is the second that says
/// whether the oracle is doing anything.
fn the_smallest_element_is_the_worst_shape() {
    // At the `XML element count` cap, which is what makes this the worst *reachable* shape
    // rather than merely the worst per-byte one.
    let unit = Unit::new().xml(&"<A/>".repeat(99_000)).build();
    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let total = alloc::total();
    drop(reader);
    let bound = oracle_bound(unit.len());
    println!(
        "99k bare elements: input {} total_alloc {total} bound {bound} ratio {:.1}x",
        unit.len(),
        total as f64 / unit.len() as f64
    );
    assert!(
        total <= bound,
        "the smallest element costs {total} bytes against a {bound}-byte bound — the shape \
         `ALLOC_MULTIPLE` is derived from, so this failing means the derivation is stale"
    );
}

/// **A one-mebibyte `id` attribute on an image reached by 256 references.**
///
/// The sibling of the shapes above, and the last of the family: sharing the *occurrence* made
/// a memo hit cheaper than a rebuild but still cloned the `Header`'s own owned strings, so an
/// attribute at exactly the `Attribute value length` cap was copied once per reference —
/// 262 MB retained from a one-megabyte header. `image_id`, `image_uuid` and the mosaic
/// pattern are `Arc<str>` for that reason; the accessors still hand out `&str`, so nothing
/// about the API changed.
fn a_large_attribute_on_a_shared_image() {
    let big = "i".repeat(1 << 20);
    let refs = r#"<Reference ref="im"/>"#.repeat(256);
    let unit = Unit::new()
        .xml(&format!(
            r#"<Image uid="im" id="{big}" geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data></Image>{refs}"#
        ))
        .build();

    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let (total, peak) = (alloc::total(), alloc::peak());
    drop(reader);
    let bound = oracle_bound(unit.len());
    println!(
        "1 MiB id x 256 references: input {} total_alloc {total} peak {peak} bound {bound}",
        unit.len()
    );
    assert!(
        total <= bound,
        "a shared image's large attribute cost {total} bytes against a {bound}-byte bound; \
         it cost 262 MB before"
    );
}

/// **One `<FITSKeyword>` reached through 49 000 `<Reference>` elements.**
///
/// § XISF decisions requires every reference to be reported as its own occurrence, and each
/// was built from the text: a one-megabyte header — inside every shipped cap — allocated
/// **2.08 GB and retained 1.05 GB**, which is process death at tier 1 from a file a consumer
/// would fetch as an untrusted prefix. That is invariant I5's third clause, not its first two:
/// no panic, no hang, just unbounded allocation.
///
/// The occurrences now share one built `Keyword` behind an `Arc`, and the memo is consulted
/// before the value is unquoted — the copy that made the gigabyte.
fn one_keyword_reached_through_many_references() {
    let big = "v".repeat(21_000);
    let refs = r#"<Reference ref="k"/>"#.repeat(49_000);
    let unit = Unit::new()
        .xml(&format!(
            r#"<FITSKeyword uid="k" name="N" value="{big}" comment=""/><Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>{refs}</Image>"#
        ))
        .build();

    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let (total, peak) = (alloc::total(), alloc::peak());
    drop(reader);
    println!(
        "49k references to one keyword: input {} total_alloc {total} peak {peak}",
        unit.len()
    );
    // Both are asserted: the peak is what "retains a gigabyte" meant and what the sharing
    // fixes outright, and the total is what the oracle measures.
    assert!(
        peak < 40_000_000,
        "references to one keyword retain {peak} bytes; they shared 1.05 GB before"
    );
    assert!(
        total <= oracle_bound(unit.len()),
        "references to one keyword cost {total} bytes against a {}-byte bound",
        oracle_bound(unit.len())
    );
}

/// **One root `<Metadata>` property, 256 images.**
///
/// §11.4 applies root metadata to every image, and the merged per-image list cloned it: a
/// single one-megabyte property across 256 images — both inside their caps — allocated and
/// retained 262 MB from a one-megabyte header. Every image shares the one list the walk built,
/// whether or not it adds properties of its own.
fn root_metadata_shared_across_many_images() {
    let value = "p".repeat(1_000_000);
    let images = distinct_images(256, "");
    let unit = Unit::new()
        .xml(&format!(
            r#"<Metadata><Property id="X" type="String" value="{value}"/></Metadata>{images}"#
        ))
        .build();

    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let total = alloc::total();
    drop(reader);
    let bound = oracle_bound(unit.len());
    println!(
        "1 MB metadata property x 256 images: input {} total_alloc {total} bound {bound}",
        unit.len()
    );
    assert!(
        total <= bound,
        "root metadata cost {total} bytes against a {bound}-byte bound; it cost 262 MB before"
    );
}

/// 49 000 `<FITSKeyword>` elements, three attributes each — 147 000 attributes over a 2.4 MB
/// header.
///
/// This is the shape that costs the most per input byte: an attribute is a name and a value,
/// and a header of nothing but attributes is a header of nothing but small strings. A `String`
/// per element name and per attribute name and value allocated 30.8 MB from this input against
/// the 27.6 MB the bound allows; interned into one arena it costs 22.5 MB.
fn attribute_heavy_header() {
    let mut xml = String::new();
    for i in 0..49_000u32 {
        xml.push_str(&format!(
            r#"<FITSKeyword name="K{i:05}" value="1" comment=""/>"#
        ));
    }
    let unit = Unit::new().xml(&xml).build();
    measure("49k FITSKeyword (root)", &unit);
}

/// The same keyword count **inside an `<Image>`**, which is the shape that still exceeds the
/// bound, and the reason this is a separate case rather than a variation of the one above.
///
/// Interning the XML strings covers the *document*. A `FITSKeyword` under an `<Image>` is also
/// materialized a second time, as a `Keyword` in that image's keyword list — metadata § The
/// API requires be reported verbatim and reachable, so it is a real second buffer and not
/// waste. Two rounds of work have shrunk it: the three texts are packed into one allocation
/// rather than three `String`s, and the walk borrows them from the arena instead of copying
/// them first. That took this shape from 36.1 MB to 31.4 MB against a 27.6 MB bound.
///
/// It was over the bound when the multiple was 8 — a figure derived from the pixel path, which
/// never accounted for a header's per-element cost. § Fuzzing records why the multiple is 32
/// and what was reduced before it moved; this asserts the bound like every other shape here.
fn keywords_under_an_image_are_materialized_twice() {
    let mut xml = String::new();
    for i in 0..49_000u32 {
        xml.push_str(&format!(
            r#"<FITSKeyword name="K{i:05}" value="1" comment=""/>"#
        ));
    }
    let unit = Unit::new()
        .xml(&format!(
            r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>{xml}</Image>"#
        ))
        .build();

    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.clone()));
    let total = alloc::total();
    drop(reader);
    let bound = oracle_bound(unit.len());
    println!(
        "49k FITSKeyword (in an Image): input {} total_alloc {total} bound {bound}",
        unit.len()
    );
    assert!(
        total <= bound,
        "keywords under an image cost {total} bytes against a {bound}-byte bound"
    );
}

/// 40 000 `<Image>` elements — an element-heavy header rather than an attribute-heavy one,
/// where the per-`Node` cost is what shows.
///
/// The occurrence walk is bounded separately, by `images_per_source`, so what this measures is
/// the parse: forty thousand nodes, their names, and two attributes apiece. Owned strings cost
/// 25.6 MB against the 24.1 MB the bound allows; interned, 18.8 MB.
fn element_heavy_header() {
    let mut xml = String::new();
    for _ in 0..40_000u32 {
        xml.push_str(r#"<Image geometry="16:16:1" sampleFormat="UInt16"/>"#);
    }
    let unit = Unit::new().xml(&xml).build();
    measure("40k Image", &unit);
}

/// Construct a `Reader` over the bytes and assert the oracle's bound over that construction.
///
/// The header parse happens in the constructor — an over-guarded header is a unit-level fault,
/// so there is no `Reader` to walk — which is why this is the whole measured region.
// With neither format compiled in, `Reader` holds `Inner::NoFormat` instead of a boxed
// decoder and so carries no drop glue at all, which is what clippy reports. The call still
// says what the measurement needs it to say in every configuration that decodes anything.
#[cfg_attr(
    not(any(feature = "fits", feature = "xisf")),
    allow(clippy::drop_non_drop)
)]
fn measure(shape: &str, unit: &[u8]) {
    let bound = oracle_bound(unit.len());

    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit)).expect("the header parses");
    let total = alloc::total();
    let peak = alloc::peak();
    drop(reader);

    println!(
        "{shape}: input {} total_alloc {total} peak {peak} bound {bound}",
        unit.len()
    );
    assert!(
        total <= bound,
        "{shape}: constructing a Reader over a {}-byte input allocated {total} bytes, above the \
         fuzz oracle's {bound}-byte bound (ALLOC_MULTIPLE x input + 8 MiB). Presume this is a bug in the \
         allocation, not a bad bound: name the buffer that allocated and show why it is \
         legitimate before touching either figure.",
        unit.len()
    );
}

/// **One `CONTINUE` chain at the assembled-value cap, shared by 256 distinct images.**
///
/// The corner of `growth_over_chain_bytes_and_distinct_images`, kept as its own shape because
/// it is the defect that really happened. The keyword fold runs once per distinct `<Image>`
/// and the `Cache` memo covers the individual `Keyword` records rather than the assembly, so
/// 256 images each carrying `<Reference ref="k0"/><Reference ref="k1"/>` to one half-megabyte
/// opener and one half-megabyte continuation each built their own megabyte-long `String`:
/// **1.03 GB allocated and 264 MB held live from a 1.05 MB header**, 982× its input. The same
/// fixture measures 11.4× once the assembly is shared, which is what the ceiling below pins.
///
/// The assembled chain is memoized on the records it is assembled from — the opener and the
/// continuations, by node — which is what makes two images carrying the same references share
/// one assembled value whatever else they carry. The images here differ, deliberately: a memo
/// keyed on more than that reads as a fix against 256 copies of one image and does nothing at
/// all against 256 images that differ by a keyword.
fn one_continue_chain_assembled_by_many_distinct_images() {
    let pad = "v".repeat(500_000);
    let images = distinct_images(256, r#"<Reference ref="k0"/><Reference ref="k1"/>"#);
    let unit = Unit::new()
        .xml(&format!(
            r#"<FITSKeyword uid="k0" name="LONGSTR" value="'{pad}&amp;'" comment=""/><FITSKeyword uid="k1" name="CONTINUE" value="'{pad}'" comment=""/>{images}"#
        ))
        .build();
    measure_multiplied(
        "1 MB shared CONTINUE chain x 256 distinct images",
        &unit,
        14.0,
    );
}

/// **The correctness twin of the shape above**, and the only assertion here that is not about
/// a byte count.
///
/// Every measurement in this file is served by a memo, and a memo is two failures away from
/// being useful: too wide a key and it never hits — which is the shape above, measured at 982×
/// its input — and too narrow a key and it hits when it should not, which no allocation
/// figure can see at all. The assembled chain is keyed on the opening record's node and
/// reported origin and on each continuation's node in order, so this asserts one image of each
/// kind that key has to separate and one pair it has to join.
///
/// The fourth image is the one the origin is in the key for: it carries its own copy of the
/// same two records as children, so it assembles the same text and must still report it as the
/// image's own card rather than as a `Reference`.
fn a_shared_chain_says_what_each_image_actually_carries() {
    let image = |children: &str| {
        format!(
            r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>{children}</Image>"#
        )
    };
    let unit = Unit::new()
        .xml(&format!(
            concat!(
                r#"<FITSKeyword uid="k0" name="LONGSTR" value="'AAA&amp;'" comment="opener"/>"#,
                r#"<FITSKeyword uid="k1" name="CONTINUE" value="'BBB'" comment="last"/>"#,
                r#"<FITSKeyword uid="k2" name="CONTINUE" value="'CCC'" comment=""/>"#,
                "{}{}{}{}"
            ),
            image(r#"<Reference ref="k0"/><Reference ref="k1"/>"#),
            // The same two records, under an image nothing else matches: the pair that must
            // share, and the shape a key over the whole record list stopped sharing.
            image(r#"<FITSKeyword name="U" value="1" comment=""/><Reference ref="k0"/><Reference ref="k1"/>"#),
            // The same opener, a different continuation: the pair that must not share. Its
            // comment falls back to the opener's, `k2` carrying none.
            image(r#"<Reference ref="k0"/><Reference ref="k2"/>"#),
            // The same text, carried directly: the same value, a different origin.
            image(
                r#"<FITSKeyword name="LONGSTR" value="'AAA&amp;'" comment="opener"/><FITSKeyword name="CONTINUE" value="'BBB'" comment="last"/>"#
            ),
        ))
        .build();

    let mut reader = Reader::seekable(Cursor::new(unit)).expect("the unit constructs");
    let mut folded = Vec::new();
    loop {
        match reader.next_image() {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => panic!("the fixture walks to its end: {error}"),
        }
        let header = reader.header().expect("a header at an image position");
        let keyword = header
            .keywords()
            .into_iter()
            .find(|k| k.name() == "LONGSTR")
            .expect("every image here carries the chain");
        folded.push((
            keyword.value().to_owned(),
            keyword.comment().map(str::to_owned),
            keyword.origin(),
        ));
    }
    let reference = KeywordOrigin::Reference;
    assert_eq!(
        folded,
        vec![
            ("AAABBB".to_owned(), Some("last".to_owned()), reference),
            ("AAABBB".to_owned(), Some("last".to_owned()), reference),
            ("AAACCC".to_owned(), Some("opener".to_owned()), reference),
            (
                "AAABBB".to_owned(),
                Some("last".to_owned()),
                KeywordOrigin::Image
            ),
        ],
        "the assembled chain each image reports"
    );
}

/// **A primary header at the `FITS header cards` cap, reported by 256 image extensions.**
///
/// The FITS half of the family, and the first FITS shape this file has ever measured. Both
/// headers' cards are reported at an extension (§ FITS decisions), so the primary's list is a
/// root list applying to every image — and it was concatenated into an owned `Vec` per
/// extension. A primary carrying 4090 `HISTORY` cards followed by 256 zero-width `IMAGE`
/// extensions allocated **53.4 MB and held 52.5 MB live from a 1.07 MB input**.
///
/// Reported as a `Keywords<'_>` view over the two pieces instead, the way `Properties<'_>`
/// serves the XISF merge. Both the total and the live peak are asserted, the defect having been
/// retention rather than churn.
fn a_full_primary_header_reported_by_many_extensions() {
    let mut primary = Hdu::primary().card("BITPIX", "8").card("NAXIS", "0");
    for i in 0..4090 {
        primary = primary.commentary("HISTORY", &format!("card {i:06}"));
    }
    let mut hdus = vec![primary];
    for i in 0..256 {
        hdus.push(extension_header(i).card("NAXIS2", "1"));
    }
    let unit = common::file(&hdus);

    alloc::reset();
    let mut reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let mut kept = Vec::new();
    while matches!(reader.next_image(), Ok(true)) {
        kept.push(
            reader
                .header()
                .expect("a header at an image position")
                .clone(),
        );
    }
    let (total, peak) = (alloc::total(), alloc::peak());
    assert_eq!(kept.len(), 256, "every extension is an image position");
    // Reported through the view, so the assertion is about what is *reachable* rather than
    // about what is stored: the count is the concatenation's, whatever holds it.
    assert_eq!(kept[0].keywords().len(), 8 + 4093);
    drop(kept);
    drop(reader);

    let bound = oracle_bound(unit.len());
    let ratio = total as f64 / unit.len() as f64;
    println!(
        "4090 primary cards x 256 extensions: input {} total_alloc {total} peak {peak} \
         bound {bound} ratio {ratio:.2}x",
        unit.len()
    );
    assert!(
        peak < 8_000_000,
        "256 extensions reporting one full primary header hold {peak} bytes live; they held \
         52.5 MB before the report became a view"
    );
    assert!(
        total <= bound,
        "256 extensions reporting one full primary header cost {total} bytes against a \
         {bound}-byte bound"
    );
    assert!(
        ratio <= 8.0,
        "256 extensions reporting one full primary header allocated {ratio:.2}x their {} input \
         bytes; the primary's card list is being copied per extension again",
        unit.len()
    );
}

// ==================================================================== the growth property
//
// Everything above is a *list of shapes*, and that list has been the problem rather than the
// guard. Each round found allocation sites the list did not name, each fix appended the shapes
// it had just been shown, and the next round found more — repeatedly, with the worst at 10 GB
// from a 1.7 MB header. The shapes are worth keeping as
// regression tests for defects that really happened, but they cannot catch the next one,
// because a list of instances never covers a class.
//
// This is the class stated directly instead. Every defect in the family had one form:
// something proportional to the input multiplied by something bounded only by a cap. So the
// property is **linear growth** — double the multiplier and the allocation should roughly
// double, because the input roughly doubles too. Anything super-linear is the class, whatever
// reader or field happens to cause it, and whether it is retained or transient.
//
// # The instrument, and why it is neither a ratio nor a growth factor
//
// Two earlier spellings of this property were both too weak, and their weaknesses are the
// reason for the shape below.
//
// **Allocation growth against input growth, both measured from the first point.** A fixed
// per-document cost is amortized differently at each size, so the tolerance had to be
// generous — and a generous tolerance compared across a wide span is a very weak statement:
// `slack = 3.0` over a 16x input span admits `n^1.4` outright, because `16^1.4 = 46 <= 48`.
// The span is what does the damage: the wider the axis runs, the more super-linear a curve
// may be and still pass.
//
// **The marginal rate.** `Δalloc / Δinput` between consecutive points — the extra bytes
// allocated per extra input byte. For an honest cost `alloc = fixed + c * input` this is
// exactly `c` at every step, whatever the fixed term is and however wide the span, so the
// tolerance no longer has to pay for amortization and can be tight. For `alloc ∝ input^1.4`
// it grows without bound. That is the figure asserted here.
//
// # One dimension is not enough, and that is not a tolerance problem
//
// The instrument above still measures one axis at a time, and the class does not live on one
// axis. **Every real instance of it is a product of two multipliers** — `Attribute value
// length` times `Images per source`, `FITS header cards` times `Images per source` — and a
// one-dimensional slice cannot see a product: pin the other multiplier and `a * b` is linear
// in `a`, with a perfectly flat marginal rate. Four such slices passed while two shapes of
// exactly this form allocated a gigabyte apiece.
//
// So the property is stated over a **grid**. What must hold is not merely that the marginal
// rate is flat along each axis, but that *the rate itself does not depend on the other
// multiplier*: the bytes allocated per extra image must be the same whether the thing those
// images share is four kilobytes or four hundred. That is the product stated directly, and it
// is the invariant the code shapes serve —
//
//     no per-occurrence allocation may be sized by a cap;
//     it must be sized by that occurrence's own input bytes.
//
// A grid of `rows x cols` measurements gives every marginal rate in both directions; the
// assertion is that none of them exceeds the rate in the cheapest corner by more than `slack`.

/// How far a marginal rate may sit above the cheapest corner's, on every axis here.
///
/// **1.5, not the 3.0 an earlier spelling used**, and the two figures are not comparable:
/// 3.0 bounded the *growth factor from the first point*, which over the 16x input span these
/// axes run admits `n^1.4` outright. Against a marginal rate the tolerance no longer pays for
/// amortization — an honest affine cost has the same rate at every step — so what is left to
/// cover is the variation between one mixture of added elements and another. It admits at most
/// `n^1.15` across a slice, which is a property of the tolerance and the span and does not
/// move with what is measured.
///
/// **The measured worst is 1.20**: `a shared CONTINUE chain x distinct images` along cols,
/// 10.08 against a base of 8.37. The base is the *cheapest* step rather than the first one,
/// which is what that figure accounts for: measuring from where the curve is smallest rather
/// than from wherever it happened to start.
///
/// It is not a decorative tightening. The FITS defect below was caught at 41.04 bytes per input
/// byte against a 13.47 base — which clears a 3.0 tolerance by one and a half per cent. A
/// number that would have passed a gigabyte-scale defect by that margin is not a bound.
const SLACK: f64 = 1.5;

/// One measured cell: the fixture's length, and what reading its header allocated.
type Cell = (usize, usize);

/// Construct a `Reader` over the bytes, which is the whole header parse for XISF, and check
/// that the fixture is the size it claims to be.
///
/// Construction is asserted rather than discarded. A fixture that silently stops constructing
/// — a cap moved, an attribute misspelt — measures the cost of the refusal instead of the cost
/// of the parse, which leaves the property green and vacuous.
///
/// So is the **image count**, and for the same reason one layer along: the multiplier every
/// grid below grows is an image count, and a fixture that stops producing images produces a
/// flat curve rather than a failure. The walk happens after the total is read, so it costs the
/// measurement nothing.
fn read_header(unit: Vec<u8>, images: usize) -> Cell {
    let len = unit.len();
    alloc::reset();
    let mut reader = Reader::seekable(Cursor::new(unit)).expect("the growth fixture constructs");
    let total = alloc::total();
    let mut walked = 0;
    loop {
        match reader.next_image() {
            Ok(true) => walked += 1,
            Ok(false) => break,
            Err(error) => panic!("the growth fixture walks to its end: {error}"),
        }
    }
    assert_eq!(walked, images, "the fixture's image count");
    drop(reader);
    (len, total)
}

/// Construct a `Reader` and advance to every image position, keeping every header.
///
/// The FITS measurement, and it has to walk: a FITS `Header` is built at the advance onto the
/// extension, not during construction, so a constructor-only measurement of a multi-extension
/// file measures the primary header alone. The headers are kept because a consumer collecting
/// them is the case that hurts — the defect this catches copied the primary's whole card list
/// into every extension's header, which is retention rather than churn.
///
/// **The walk is asserted as hard as the construction is.** `while matches!(reader.next_image(),
/// Ok(true))` treats an `Err` as the end of the file, so a regression that truncates the walk
/// at the first extension makes every row of a grid measure the same short walk — three
/// identical rows, a perfectly flat rate, and a property that has stopped measuring anything.
/// The count the fixture was built with is passed in and checked.
fn walk_headers(unit: Vec<u8>, images: usize) -> Cell {
    let len = unit.len();
    alloc::reset();
    let mut reader = Reader::seekable(Cursor::new(unit)).expect("the growth fixture constructs");
    let mut kept = Vec::new();
    loop {
        match reader.next_image() {
            Ok(true) => kept.push(
                reader
                    .header()
                    .expect("a header at an image position")
                    .clone(),
            ),
            Ok(false) => break,
            Err(error) => panic!("the growth fixture walks to its end: {error}"),
        }
    }
    let total = alloc::total();
    assert_eq!(kept.len(), images, "the fixture's image count");
    drop(kept);
    drop(reader);
    (len, total)
}

/// Grow one multiplier and measure the header parse at each count.
///
/// `images` is how many image positions the fixture has at that count, which is not always the
/// count itself: an axis may grow references to one image, or grow a chain under a single one.
fn growth_curve(
    build: impl Fn(usize) -> Vec<u8>,
    images: impl Fn(usize) -> usize,
    counts: &[usize],
) -> Vec<Cell> {
    counts
        .iter()
        .map(|&n| read_header(build(n), images(n)))
        .collect()
}

/// The marginal allocation rate between consecutive measurements: extra bytes allocated per
/// extra input byte.
///
/// Signed. The counting allocator can report a step that allocated slightly *less* than its
/// predecessor — the parse is not perfectly monotonic in fixture size — and the earlier
/// spelling saturated that to zero, which is not the harmless reading it looks like: the base
/// below is the cheapest rate on the curve, a zero base makes `base * slack` zero, and every
/// honest step beside it then fails. [`cheapest_rate`] skips non-positive steps instead, which
/// it can only do if they are still visible as such.
fn marginal_rates(curve: &[Cell]) -> Vec<f64> {
    curve
        .windows(2)
        .map(|pair| {
            let ((prev_len, prev_total), (len, total)) = (pair[0], pair[1]);
            assert!(
                len > prev_len,
                "a growth axis must grow the input: {prev_len} then {len}"
            );
            (total as f64 - prev_total as f64) / (len - prev_len) as f64
        })
        .collect()
}

/// The rate every other rate is measured against: the **cheapest** step, which is what "the
/// cheapest corner" has always claimed and what the first step is not.
///
/// The first step is the one the fixture's fixed costs land on hardest, because it is the
/// smallest increment on the axis. Measured, on `root references to one image` as it was
/// written: step 0 cost 82.19 bytes per input byte and step 1 cost 16.69, so taking step 0 as
/// the base made the effective tolerance on that axis 7.4x rather than the 1.5x [`SLACK`]
/// states. A tolerance that varies with which end of the axis is cheapest is not a tolerance.
///
/// Non-positive steps are skipped rather than used. A step that allocated less than its
/// predecessor is never the defect this property exists to catch, and it cannot serve as a
/// base: the assertion is an upper bound, and zero is not one. An axis on which *no* step
/// allocated anything measures nothing at all, so that is a failure rather than a pass.
fn cheapest_rate(axis: &str, rates: &[f64]) -> f64 {
    let base = rates
        .iter()
        .copied()
        .filter(|rate| *rate > 0.0)
        .fold(f64::INFINITY, f64::min);
    assert!(
        base.is_finite(),
        "{axis}: no step on this axis allocated more than the one before it, so the axis \
         measures nothing. The fixture has stopped growing what it means to grow."
    );
    base
}

/// Assert that allocation grows no faster than the input does, along one multiplier axis.
///
/// Every rate on the curve measures the same mixture of added elements, so a fixed
/// per-document cost and a large shared payload both cancel out of the comparison instead of
/// having to be paid for in slack. What is left for the tolerance to cover is the variation
/// between one mixture and another.
fn assert_growth_is_linear(axis: &str, curve: &[Cell], slack: f64) {
    let rates = marginal_rates(curve);
    let base = cheapest_rate(axis, &rates);
    assert_rates_are_flat(axis, &rates, base, slack);
}

/// The shared tail of both assertions: no marginal rate may exceed `base` by more than `slack`.
fn assert_rates_are_flat(axis: &str, rates: &[f64], base: f64, slack: f64) {
    for (step, &rate) in rates.iter().enumerate() {
        println!("{axis}: step {step} costs {rate:.2} bytes per input byte (base {base:.2})");
        assert!(
            rate <= base * slack,
            "{axis}: step {step} allocated {rate:.2} bytes per extra input byte against \
             {base:.2} in the cheapest corner, above the {slack:.1}x tolerance. Something here \
             is sized by a cap rather than by the input that reached it — that is the defect \
             class this property exists to catch, and the shape lists above will not name it \
             for you."
        );
    }
}

/// Measure a two-multiplier grid and assert the marginal rate in **both** directions.
///
/// `build(row, col)` takes both multipliers. Every row is a curve along `cols` and every
/// column is a curve along `rows`, so the far edges — the corner where both multipliers are
/// large — are measured rather than inferred from two slices through the origin.
///
/// Each direction is normalized against its own cheapest corner, because the two directions
/// buy different things per input byte: an extra `<Image>` element costs a whole `Header`
/// against a hundred-odd source bytes, while an extra kilobyte of one attribute costs about a
/// kilobyte. What must be flat is each direction's rate *across the other multiplier*.
fn assert_growth_over_the_grid(
    axis: &str,
    build: impl Fn(usize, usize) -> Vec<u8>,
    measure: impl Fn(Vec<u8>, usize) -> Cell,
    rows: &[usize],
    cols: &[usize],
    slack: f64,
) {
    // `rows` is the image count on every grid here, which is what `measure` checks the
    // fixture against.
    let grid: Vec<Vec<Cell>> = rows
        .iter()
        .map(|&r| {
            cols.iter()
                .map(|&c| {
                    let cell = measure(build(r, c), r);
                    println!("{axis}: ({r}, {c}) input {} alloc {}", cell.0, cell.1);
                    cell
                })
                .collect()
        })
        .collect();

    // Along `cols`, with each row pinned. The base is the cheapest step in this direction,
    // wherever on the grid it falls.
    let along_cols: Vec<Vec<f64>> = grid.iter().map(|row| marginal_rates(row)).collect();
    let base = cheapest_rate(axis, &along_cols.concat());
    for (i, rates) in along_cols.iter().enumerate() {
        assert_rates_are_flat(
            &format!("{axis} along cols at row {}", rows[i]),
            rates,
            base,
            slack,
        );
    }

    // Along `rows`, with each column pinned — the direction the product hides in: pin the
    // shared payload and a per-image copy of it is perfectly linear in the image count, and
    // only its *rate* betrays that it was sized by the payload rather than by the image.
    let along_rows: Vec<Vec<f64>> = (0..cols.len())
        .map(|j| {
            let column: Vec<Cell> = grid.iter().map(|row| row[j]).collect();
            marginal_rates(&column)
        })
        .collect();
    let base = cheapest_rate(axis, &along_rows.concat());
    for (j, rates) in along_rows.iter().enumerate() {
        assert_rates_are_flat(
            &format!("{axis} along rows at col {}", cols[j]),
            rates,
            base,
            slack,
        );
    }
}

/// One `<Image uid>` reached by a growing number of root-level `<Reference>` elements — the
/// one axis here whose subject *is* the sharing, so the image it repeats is the same node by
/// construction.
///
/// **The counts are powers of two, and they stay well inside `Images per source`.** Both halves
/// of that are load-bearing on this axis, and neither was true when it read 82.19 then 16.69
/// and called the first figure the cheapest corner.
///
/// A reference costs twenty-one input bytes and pushes a whole `Occurrence` — some 356 bytes —
/// onto a `Vec`, so what this axis measures is dominated by that vector's amortized growth.
/// Cumulative allocation for a doubling `Vec` is a staircase: it is linear in the count when
/// read from the origin and flat between two doublings, so a *marginal* rate is comparable
/// across steps only where the endpoints sit at the same phase of it. `n + 1` occurrences at
/// `n = 2^k` puts every endpoint one past a doubling, and the steps then read the same figure
/// to the second decimal. At 128, 200 and 240 references, where the vector has already reached
/// its last capacity, they read 82.19 and then 14.38 — the same allocation, sampled off-phase.
///
/// Past the cap they measure something else again: `Images per source` is a refusal at
/// `next_image()`, not a truncation, so 256 references is a fixture that no longer walks.
fn growth_along_root_references() {
    let payload = "v".repeat(4096);
    let curve = growth_curve(
        |n| {
            Unit::new()
                .xml(&format!(
                    r#"<Image uid="im" id="{payload}" geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data></Image>{}"#,
                    r#"<Reference ref="im"/>"#.repeat(n)
                ))
                .build()
        },
        // The element itself is an occurrence too, and the whole is clamped at the cap.
        |n| (n + 1).min(256),
        &[16, 64, 128],
    );
    assert_growth_is_linear("root references to one image", &curve, SLACK);
}

/// A growing number of **distinct** images each referencing one root element — the multiplier
/// the shape list was blind to.
fn growth_along_distinct_images() {
    let payload = "p".repeat(4096);
    let curve = growth_curve(
        |n| {
            Unit::new()
                .xml(&format!(
                    r#"<Property id="P" type="String" value="{payload}"/>{}"#,
                    distinct_images(n, r#"<Reference ref="p"/>"#)
                ))
                .build()
        },
        |n| n,
        &[16, 64, 256],
    );
    assert_growth_is_linear("distinct images referencing one property", &curve, SLACK);
}

/// A growing `CONTINUE` chain — the axis with no `<Reference>` in it at all, which is how two
/// of loop nine's three vectors reached their multiplier.
fn growth_along_a_continue_chain() {
    let opener = "o".repeat(4096);
    let curve = growth_curve(
        |n| {
            Unit::new()
                .xml(&format!(
                    r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data><FITSKeyword name="LONGSTR" value="'{opener}&amp;'" comment=""/>{}</Image>"#,
                    r#"<FITSKeyword name="CONTINUE" value="'x&amp;'" comment=""/>"#.repeat(n)
                ))
                .build()
        },
        // One image, however long the chain under it grows.
        |_| 1,
        &[64, 256, 1024],
    );
    assert_growth_is_linear("a growing CONTINUE chain", &curve, SLACK);
}

/// Root `<Metadata>` properties beside a growing number of images that each add one of their
/// own — the branch that does not take the shared list.
fn growth_along_metadata_with_own_properties() {
    let metadata: String = (0..512)
        .map(|i| format!(r#"<Property id="M{i}" type="String" value="m"/>"#))
        .collect();
    let curve = growth_curve(
        |n| {
            Unit::new()
                .xml(&format!(
                    "<Metadata>{metadata}</Metadata>{}",
                    distinct_images(n, r#"<Property id="own" type="String" value="o"/>"#)
                ))
                .build()
        },
        |n| n,
        &[16, 64, 256],
    );
    assert_growth_is_linear(
        "root metadata beside images with own properties",
        &curve,
        SLACK,
    );
}

/// **The one shape that exceeds the oracle's bound on purpose** — `<Reference>` elements
/// composing a *distinct* `CONTINUE` chain for each of many images.
///
/// Every other shape in this file asserts the fuzz oracle's bound. This one asserts that it
/// is **over** it, because the allocation is the reporting contract rather than a defect, and
/// a record that says so is worth less than a test that fails when it stops being true.
///
/// `c` components with `k` alternatives each compose `k^c` images. Each image assembles a
/// genuinely different value, so nothing can be shared and the amplification is `images / k`,
/// maximized at the smallest `k`. At `k = 2` and `c = 8` that is 128× before the assembly's
/// own overhead, and the ceiling is `Images per source × Assembled keyword value × ~3.8` —
/// about **1.05 GB from a 2.2 MB input** at the shipped caps, which § The caps states as the
/// crate's joint cap-product figure and § Fuzzing derives.
///
/// Measured here at a 64 KiB assembled value rather than the 1 MiB cap: the figure is linear
/// in that size, so a sixteenth of it measures the same shape for a sixteenth of the memory,
/// which matters on the shared runners. Extrapolate, do not re-measure at 1 MiB in CI.
///
/// **Why this is not raised away.** Repeated-assembly defects — the class this file otherwise
/// exists to catch — allocate exactly the same `images × assembled value`. They differ only in
/// what the pool costs: theirs is one value's worth of input, this one's is `k`. So the ratios
/// differ by exactly `k`, and at `k = 2` the entire discrimination window is a factor of two:
/// the last such defect ran at 982.5× against this shape's 485.4×. An `ALLOC_MULTIPLE` above
/// 485 admits repeated assembly at 970×, which is the oracle switched off for the class.
///
/// The fix that would end it is not a cap. It is representing an assembled value as spans into
/// a retained header buffer, materialized on demand — a public API change, deferred.
fn composed_chains_across_distinct_images() {
    const COMPONENTS: usize = 8;
    const ASSEMBLED: usize = 64 << 10;
    let images = 1usize << COMPONENTS; // k = 2

    let pad = "v".repeat(ASSEMBLED / COMPONENTS - 8);
    let mut xml = String::new();
    for c in 0..COMPONENTS {
        for o in 0..2 {
            // The opener carries the chain's name; the rest continue it. Every component but
            // the last ends in `&`, which is what makes the next record part of the chain.
            let name = if c == 0 { "LONGSTR" } else { "CONTINUE" };
            let amp = if c + 1 < COMPONENTS { "&amp;" } else { "" };
            xml.push_str(&format!(
                r#"<FITSKeyword uid="c{c}o{o}" name="{name}" value="'{pad}{amp}'" comment=""/>"#
            ));
        }
    }
    // Image `i` reaches component `c`'s alternative `(i >> c) & 1`, so the images enumerate
    // every combination exactly once and no two assemble the same value.
    for i in 0..images {
        xml.push_str(
            r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>"#,
        );
        for c in 0..COMPONENTS {
            xml.push_str(&format!(r#"<Reference ref="c{c}o{}"/>"#, (i >> c) & 1));
        }
        xml.push_str("</Image>");
    }
    let unit = Unit::new().xml(&xml).build();

    alloc::reset();
    let reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let (total, peak) = (alloc::total(), alloc::peak());
    drop(reader);

    // `images × assembled` is the text the contract requires be reported. The multiple over it
    // is the assembly's own overhead — the accumulator, the packed `Keyword`, and one copy per
    // continuation — measured at 4.01 and pinned at 4.5, which is
    // tight on purpose: the chain-memo defect this file's other shapes catch put this one at
    // 4.88, so the pin catches that regression here too. The count is a deterministic
    // allocation total, not a timing measurement, so there is no variance to leave room for. Halving it is possible
    // (returning `Cow` from `unquote`, assembling straight into the packed buffer) and would
    // not bring the shape under the oracle's bound; see the doc comment.
    let contract = images * ASSEMBLED;
    let ceiling = contract * 9 / 2;
    let bound = oracle_bound(unit.len());
    println!(
        "composed CONTINUE chains x {images} distinct images: input {} total_alloc {total} \
         peak {peak} contract {contract} ceiling {ceiling} oracle_bound {bound} ratio {:.2}x",
        unit.len(),
        total as f64 / unit.len() as f64
    );

    assert!(
        total <= ceiling,
        "composed chains: allocated {total} bytes against a {ceiling}-byte ceiling — \
         {:.2}x the {contract} bytes of assembled text the reporting contract requires, above \
         the 4.5x this shape is pinned at. The oracle's bound does not catch this shape (see \
         below), so this is the assertion that says the assembly's overhead grew.",
        total as f64 / contract as f64
    );
    assert!(
        total > bound,
        "composed chains: allocated {total} bytes, within the oracle's {bound}-byte bound. \
         That is good news and a stale record: § Fuzzing and § The caps both state this shape \
         exceeds the bound and that the crate's joint cap-product ceiling is about 1.05 GB. \
         Re-derive both, then delete this assertion."
    );
}

// ------------------------------------------------------------------ the two-multiplier grids
//
// The axes above grow one multiplier each. These grow two, which is the shape every real
// instance of the class has actually had.

/// **`Assembled keyword value` x `Images per source`** — a shared `CONTINUE` chain against the
/// number of distinct images that assemble it.
///
/// The `Cache` memo shares individual `Keyword` records, and the four slices above all passed
/// because of it. What sits outside it is the **assembly**: the fold runs once per distinct
/// `<Image>`, and the occurrence memo covers only reference-reached images, so 256 distinct
/// images each carrying two `<Reference>` elements to one half-megabyte opener and one
/// half-megabyte continuation each assembled their own megabyte-long `String`. It allocated
/// 1.03 GB and held 264 MB live from a 1.05 MB header — 982x its input, with every count
/// inside its cap.
///
/// Neither multiplier is visible on its own. Pin the payload and the cost is exactly linear in
/// the image count; pin the image count and it is exactly linear in the payload. It is the
/// *rate* along one that is sized by the other, which is what the grid measures.
///
/// **And neither is visible at all if the images are the same image.** A grid built from
/// `image.repeat(images)` reads 17.30, 16.30, 16.30 along its image axis while the defect
/// above sits behind it: the assembly is memoized on the whole image's record sequence,
/// byte-identical images agree on every record, and such a grid measures the memo. This one
/// builds distinct images — see [`distinct_images`] — and reverting the fix takes this
/// corner back to 982x and fails the shape assertion above.
fn growth_over_chain_bytes_and_distinct_images() {
    assert_growth_over_the_grid(
        "a shared CONTINUE chain x distinct images",
        |images, chain_bytes| {
            // Two records, so the assembled value is twice `chain_bytes` and stays inside
            // `keyword_value_bytes`. The size of the chain is the multiplier, not its length:
            // a chain long enough to matter needs one `<Reference>` per record per image,
            // which would make the *input* grow with the product and hide what is being
            // measured.
            let pad = "v".repeat(chain_bytes);
            Unit::new()
                .xml(&format!(
                    r#"<FITSKeyword uid="k0" name="LONGSTR" value="'{pad}&amp;'" comment=""/><FITSKeyword uid="k1" name="CONTINUE" value="'{pad}'" comment=""/>{}"#,
                    distinct_images(images, r#"<Reference ref="k0"/><Reference ref="k1"/>"#)
                ))
                .build()
        },
        read_header,
        &[16, 64, 256],
        &[4096, 32_768, 262_144],
        SLACK,
    );
}

/// **`FITS header cards` x `Images per source`** — a primary header's card count against the
/// number of image extensions that report it.
///
/// § FITS decisions requires both headers' cards to be reported at an extension, the
/// extension's followed by the primary's, so the primary's whole list applies to every image in
/// the file — a root list with a per-image multiplier, exactly the shape `PropertySet` exists
/// for on the XISF side. `build_header` concatenated the two into an owned `Vec` per extension:
/// a primary carrying 4090 `HISTORY` cards followed by 256 zero-width `IMAGE` extensions — one
/// 2880-byte block each, every count inside its cap — allocated 53.4 MB and held 52.5 MB live
/// from a 1.07 MB input.
///
/// This file measured no FITS shape at all, which is how that survived.
fn growth_over_primary_cards_and_image_extensions() {
    assert_growth_over_the_grid(
        "primary cards x image extensions",
        |extensions, primary_cards| {
            let mut primary = Hdu::primary().card("BITPIX", "8").card("NAXIS", "0");
            for i in 0..primary_cards {
                primary = primary.commentary("HISTORY", &format!("card {i:06}"));
            }
            let mut hdus = vec![primary];
            // `NAXIS1 = 0` is a declined image position rather than a skipped one: the reader
            // sits on it and builds its header, and the data unit is empty, so an extension
            // costs one header block and nothing else.
            //
            // `EXTVER` is what makes the extensions distinct, for the reason
            // [`distinct_images`] gives — a tenth card still fits the one 2880-byte block, so
            // the input length is unchanged and the rates stay comparable.
            for i in 0..extensions {
                hdus.push(extension_header(i).card("NAXIS2", "1"));
            }
            common::file(&hdus)
        },
        walk_headers,
        &[16, 64, 256],
        &[64, 512, 4000],
        SLACK,
    );
}

/// **`Assembled keyword value` x `Images per source`** — an inherited `ROWORDER` against the
/// number of image extensions that apply it.
///
/// The FITS twin of the chain grid above, and the instance that says a grid over one product
/// per format is not enough. `ROWORDER` is reported as *text*, `INHERIT = T` applies the
/// primary's to every extension, and a `ROWORDER` value is a keyword value — bounded by
/// `Assembled keyword value`, not by one card's eighty bytes, because a `CONTINUE` chain
/// assembles it. Classified per image position, a 240 KB assembled value across 256 extensions
/// allocated **189 MB and held 64 MB live from a 1.06 MB input**, 178x, which is over the
/// oracle's bound on a shape where every count is inside its cap.
///
/// It is a different multiplier from `growth_over_primary_cards_and_image_extensions`'s — that
/// one is the primary's card *count*, this one is one card's assembled *length* — and the fix
/// is different too: a view does nothing for a value that is classified rather than listed.
/// `RowOrder::Other` holds `Arc<str>` and the `Decoder` classifies the primary's once.
fn growth_over_inherited_row_order_and_image_extensions() {
    assert_growth_over_the_grid(
        "inherited ROWORDER x image extensions",
        |extensions, chain| common::file(&inherited_row_order_file(extensions, chain)),
        walk_headers,
        &[16, 64, 256],
        &[64, 512, 4000],
        SLACK,
    );
}

/// **An inherited `ROWORDER` at 240 KB, applied by 256 image extensions.**
///
/// The corner of the grid above, kept as its own shape for the same reason the other two are.
fn a_long_inherited_row_order_applied_by_many_extensions() {
    let unit = common::file(&inherited_row_order_file(256, 4000));

    alloc::reset();
    let mut reader = Reader::seekable(Cursor::new(unit.clone())).expect("the unit constructs");
    let mut kept = Vec::new();
    while matches!(reader.next_image(), Ok(true)) {
        kept.push(
            reader
                .header()
                .expect("a header at an image position")
                .clone(),
        );
    }
    let (total, peak) = (alloc::total(), alloc::peak());
    assert_eq!(kept.len(), 256, "every extension is an image position");
    // The report is unchanged — this is about what holds it, not about what it says.
    let RowOrder::Other(text) = kept[0].row_order().expect("FITS reports a row order") else {
        panic!("an unrecognized ROWORDER is reported verbatim");
    };
    assert_eq!(text.len(), 240_061);
    drop(kept);
    drop(reader);

    let bound = oracle_bound(unit.len());
    let ratio = total as f64 / unit.len() as f64;
    println!(
        "240 KB inherited ROWORDER x 256 extensions: input {} total_alloc {total} peak {peak} \
         bound {bound} ratio {ratio:.2}x",
        unit.len()
    );
    assert!(
        peak < 8_000_000,
        "256 extensions applying one inherited ROWORDER hold {peak} bytes live; they held \
         64 MB before the classification was shared"
    );
    assert!(
        total <= bound,
        "256 extensions applying one inherited ROWORDER cost {total} bytes against a \
         {bound}-byte bound"
    );
    assert!(
        ratio <= 8.0,
        "256 extensions applying one inherited ROWORDER allocated {ratio:.2}x their {} input \
         bytes; the inherited value is being classified per extension again",
        unit.len()
    );
}

/// A primary whose `ROWORDER` is assembled from a `CONTINUE` chain of `chain` records, followed
/// by `extensions` zero-width `IMAGE` extensions that each declare `INHERIT = T`.
///
/// The continuation cards are written raw: a conforming `CONTINUE` record carries **no value
/// indicator** (FITS 4.0 §4.2.1.2), so its text starts at byte 8, and a card built with `= `
/// is an unrecognized keyword rather than a continuation — which folds nothing and measures
/// nothing.
fn inherited_row_order_file(extensions: usize, chain: usize) -> Vec<Hdu> {
    let text = "A".repeat(60);
    let mut primary = Hdu::primary()
        .card("BITPIX", "8")
        .card("NAXIS", "0")
        .card("ROWORDER", &format!("'{text}&'"));
    for _ in 0..chain {
        primary = primary.raw(format!("CONTINUE  '{text}&'").as_bytes());
    }
    primary = primary.raw(b"CONTINUE  'z'");

    let mut hdus = vec![primary];
    for i in 0..extensions {
        hdus.push(extension_header(i).card("NAXIS2", "1").card("INHERIT", "T"));
    }
    hdus
}

/// A zero-width `IMAGE` extension that no other extension in the file is byte-identical to.
///
/// The FITS half of what [`distinct_images`] does: a file of repeated extension headers is a
/// file of repeated input, and a fixture built from repeated input is one memo away from
/// measuring nothing. `NAXIS2` is left to the caller only because it reads better beside the
/// `INHERIT` the row-order file adds.
fn extension_header(index: usize) -> Hdu {
    Hdu::extension("IMAGE")
        .card("BITPIX", "8")
        .card("NAXIS", "2")
        .card("NAXIS1", "0")
        .card("EXTVER", &format!("{index}"))
        .card("PCOUNT", "0")
        .card("GCOUNT", "1")
}
