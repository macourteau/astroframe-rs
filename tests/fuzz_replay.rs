//! The cheap half of fuzzing: replay, on **stable**.
//!
//! `cargo-fuzz` needs a nightly toolchain for its sanitizer flags while this crate's floor is
//! stable 1.88, so the exploratory lane is pinned to nightly and is allowed to lag. Keeping
//! this half on stable is what makes that separation affordable — the seeds are fed through
//! the same public entry points by a plain loop, no nightly required, and a reintroduced
//! crash fails the build on every push.
//!
//! Two sources of seeds:
//!
//! 1. **Synthetic seeds built here**, so the lane asserts something from its first run rather
//!    than waiting for the fuzzer to find a crash. Seeds come from the fixture builders and
//!    never from real frames — a fuzz corpus is the easiest place for observatory coordinates
//!    to get committed by accident.
//! 2. **Every file under `fuzz/corpus/` and `fuzz/artifacts/`**, when present. Every crash the
//!    fuzzer finds is committed as a regression seed once fixed, and this is what replays it.
//!
//! **And it asserts the allocation bound, not merely that the call returns.** Half the seeds
//! here exist for allocation defects — a keyword reached through many references, a chain
//! shared across images, a primary header reported by many extensions — and a lane that only
//! checks for a panic asserts nothing about any of them. The seeds are small, so the bound
//! they clear is not the interesting part; what matters is that the *nightly* oracle's
//! arithmetic runs on every push, over the committed corpus, on stable. A seed grown by the
//! fuzzer into a real multiplication fails here as it would fail there.
//!
//! **Everything runs in one `#[test]`**, deliberately, for the reason `tests/header_alloc.rs`
//! gives: a `#[global_allocator]` counts the whole process and the harness runs `#[test]`s on
//! parallel threads, so a second test allocating anywhere lands in the counter this one just
//! reset.

#[path = "common/alloc.rs"]
mod alloc;
mod common;

use astroframe::{Limits, Reader, Samples};
use common::Hdu;
use common::xisf::{Unit, le_u16, raw_unit, repeating_u16, zlib};
use std::io::Cursor;

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// `fuzz/src/lib.rs`'s `ALLOC_MULTIPLE`, kept in step by hand — the fuzz crate is not a
/// dependency of the test crate, the same seam `tests/header_alloc.rs` documents.
const ALLOC_MULTIPLE: usize = 32;

/// `fuzz/src/lib.rs`'s `bound()`: `ALLOC_MULTIPLE x input_length + xml_header_bytes`. The
/// header-cap term is there because a twenty-byte input may legitimately declare a header the
/// reader grows toward.
fn bound(input_len: usize) -> usize {
    ALLOC_MULTIPLE
        .saturating_mul(input_len)
        .saturating_add(limits().xml_header_bytes as usize)
}

/// `fuzz/src/lib.rs`'s `IMPLIED_CEILING`, and the one lane that is allowed it: `read_image()`
/// allocates the destination itself, so its bound carries the geometry-implied size on top of
/// the general one, clamped overall rather than per occurrence.
const IMPLIED_CEILING: usize = 1 << 24;

fn accumulate_implied(total: usize, width: u32, height: u32, channels: u32, bytes: u32) -> usize {
    let samples = u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(u64::from(channels));
    let geometry_bytes = samples.saturating_mul(u64::from(bytes.max(4)));
    total
        .saturating_add(geometry_bytes.min(IMPLIED_CEILING as u64) as usize)
        .min(IMPLIED_CEILING)
}

/// `fuzz/src/lib.rs`'s `assert_within`, with its message: a failure is a bug in the
/// allocation until someone names the buffer and shows otherwise.
fn assert_within(total: usize, bound: usize, target: &str, input_len: usize) {
    assert!(
        total <= bound,
        "{target}: allocated {total} bytes from a {input_len}-byte input, above the {bound} \
         byte bound. Presume this is a bug in the allocation, not a bad bound: name the \
         buffer that allocated and show why it is legitimate before touching ALLOC_MULTIPLE."
    );
}

/// The tightened caps every fuzz target constructs its `Reader` with. Kept identical to the
/// fuzz crate's `limits()` so a seed behaves the same in both.
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.total_samples = 1 << 18;
    limits.decoded_output_bytes = 1 << 20;
    limits
}

/// A `Read` with no `Seek`, matching the `decode_sequential` target's source.
struct Forward<'a> {
    data: &'a [u8],
    at: usize,
}

impl std::io::Read for Forward<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.data.len() - self.at).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Drive one seed through every entry point the targets reach, one measured lane at a time.
///
/// The contract is that malformed input produces an `Err`, never a panic, a hang, or
/// unbounded allocation — so an error here is an outcome, not a failure. What each lane
/// asserts is that the call *returns* and that it stayed inside the bound its target asserts.
///
/// **One lane per target rather than one pass doing everything.** The lanes have different
/// bounds — `read_image()` sizes its own destination and is allowed the geometry-implied term
/// — and summing three lanes' allocation against the loosest of their three bounds is both
/// weaker than any of them and able to fail on a shape none of them would.
fn replay(data: &[u8]) {
    header_lane(data);
    decode_any_lane(data);
    decode_alloc_lane(data);
    decode_sequential_lane(data);
}

/// The prefix `fits_header` prepends to its input, byte for byte.
const FITS_PREFIX: &[u8] = b"SIMPLE  ";

/// The signature `xisf_header` prepends, and the length of the whole preamble it writes:
/// signature, declared header length, reserved.
const XISF_SIGNATURE: &[u8] = b"XISF0100";
const XISF_PREAMBLE: usize = 16;

/// What `fits_header` and `xisf_header` actually hand to the parser: their own prefix, then
/// the corpus file.
///
/// Both targets prepend the format's signature so the fuzzer spends its budget on card and
/// XML parsing rather than on rediscovering a signature `decode_any` already explores. Their
/// corpora therefore hold what the target adds *to*: a seed carrying the signature itself is
/// parsed doubled — the FITS card grid shifted eight bytes out of alignment for the whole
/// file, the XISF header region opening with a second literal signature so the XML parse
/// fails at byte zero. [`seed_the_fuzz_corpus`] strips the prefix on the way in, and these
/// put it back so the replay lane sees the bytes the fuzz target sees.
///
/// The sibling case is `xisf_block`, whose corpus is raw units although its target takes a
/// tuple; that mismatch is deliberate and is reasoned about where the seeds are written.
fn wrap_as_fits_header(seed: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(seed.len() + FITS_PREFIX.len());
    input.extend_from_slice(FITS_PREFIX);
    input.extend_from_slice(seed);
    input
}

/// See [`wrap_as_fits_header`].
fn wrap_as_xisf_header(seed: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(seed.len() + XISF_PREAMBLE);
    input.extend_from_slice(XISF_SIGNATURE);
    input.extend_from_slice(&(seed.len() as u32).to_le_bytes());
    input.extend_from_slice(&[0u8; 4]);
    input.extend_from_slice(seed);
    input
}

/// The inverse of [`wrap_as_fits_header`], and a no-op on a seed that never carried the
/// prefix — the degenerate inputs are seeded as they are.
fn strip_fits_header(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(FITS_PREFIX).unwrap_or(bytes)
}

/// The inverse of [`wrap_as_xisf_header`], which drops any data unit along with the preamble.
///
/// The target declares a header length covering its whole input, so a block beyond the XML
/// would be declared as part of the header rather than as the block it is. Keeping the header
/// region alone is what makes the wrap exact: `xisf_header` rebuilds the same unit the seed
/// described, header-only, which is the whole of what that target parses.
fn strip_xisf_header(bytes: &[u8]) -> &[u8] {
    let Some(rest) = bytes.strip_prefix(XISF_SIGNATURE) else {
        return bytes;
    };
    let Some(declared) = rest.get(..4) else {
        return bytes;
    };
    let declared = u32::from_le_bytes(declared.try_into().expect("a four-byte slice")) as usize;
    let region = &bytes[XISF_PREAMBLE.min(bytes.len())..];
    &region[..declared.min(region.len())]
}

/// `fits_header` / `xisf_header`: the walk with no pixel read at all, which is where every
/// header-multiplication seed here does its work and the cheapest place to catch one.
fn header_lane(data: &[u8]) {
    alloc::reset();
    if let Ok(mut reader) = Reader::seekable_with_limits(Cursor::new(data), limits()) {
        let mut advances = 0;
        while advances < 1024 {
            match reader.next_image() {
                Ok(true) => advances += 1,
                Ok(false) | Err(_) => break,
            }
            let _ = reader.header();
        }
    }
    assert_within(
        alloc::total(),
        bound(data.len()),
        "walk_headers",
        data.len(),
    );
}

/// `decode_any` and `xisf_block`: a seekable source read into a destination the caller sizes
/// from the parsed header, as `f32` and as native samples.
fn decode_any_lane(data: &[u8]) {
    alloc::reset();
    if let Ok(mut reader) = Reader::seekable_with_limits(Cursor::new(data), limits()) {
        let mut advances = 0;
        while matches!(reader.next_image(), Ok(true)) && advances < 256 {
            advances += 1;
            let Some(h) = reader.header() else { break };
            if let (Some(w), Some(ht), Some(c)) = (h.width(), h.height(), h.channels()) {
                let len = (w as u64 * ht as u64 * c as u64).min(1 << 18) as usize;
                let mut dst = vec![0.0f32; len];
                let _ = reader.read_image_into(&mut dst);
                if let Some(f) = h.sample_format() {
                    let mut samples = Samples::zeroed(f, len);
                    let _ = reader.read_samples_into(&mut samples);
                }
            }
        }
    }
    assert_within(alloc::total(), bound(data.len()), "decode_any", data.len());
}

/// `decode_alloc`: the allocating `read_image()`, whose bound carries the geometry-implied
/// size the declared-versus-stored gap can produce.
fn decode_alloc_lane(data: &[u8]) {
    alloc::reset();
    let mut implied = 0usize;
    if let Ok(mut reader) = Reader::seekable_with_limits(Cursor::new(data), limits()) {
        let mut advances = 0;
        while matches!(reader.next_image(), Ok(true)) && advances < 256 {
            advances += 1;
            if let Some(h) = reader.header()
                && let (Some(w), Some(ht), Some(c), Some(f)) =
                    (h.width(), h.height(), h.channels(), h.sample_format())
            {
                implied = accumulate_implied(implied, w, ht, c, f.bytes());
            }
            let _ = reader.read_image();
        }
    }
    assert_within(
        alloc::total(),
        bound(data.len()).saturating_add(implied),
        "decode_alloc",
        data.len(),
    );
}

/// `decode_sequential`: the forward-only source, whose read-and-discard skipping and
/// no-known-length size checks no other target reaches.
fn decode_sequential_lane(data: &[u8]) {
    alloc::reset();
    if let Ok(mut reader) = Reader::sequential_with_limits(Forward { data, at: 0 }, limits()) {
        let mut advances = 0;
        while matches!(reader.next_image(), Ok(true)) && advances < 256 {
            advances += 1;
            let Some(h) = reader.header() else { break };
            if let (Some(w), Some(ht), Some(c)) = (h.width(), h.height(), h.channels()) {
                let len = (w as u64 * ht as u64 * c as u64).min(1 << 18) as usize;
                let mut dst = vec![0.0f32; len];
                let _ = reader.read_image_into(&mut dst);
            }
        }
    }
    assert_within(
        alloc::total(),
        bound(data.len()),
        "decode_sequential",
        data.len(),
    );
}

/// Seeds built here rather than committed, so no opaque blob enters the repository and no
/// real frame can leak into the corpus.
fn synthetic_seeds() -> Vec<(String, Vec<u8>)> {
    let mut seeds: Vec<(String, Vec<u8>)> = Vec::new();

    let levels: Vec<i16> = (0u16..64).map(|l| (l as i32 - 32768) as i16).collect();
    seeds.push((
        "fits/unsigned16".into(),
        common::file(&[Hdu::primary()
            .image_2d(16, 8, 8)
            .unsigned_convention(16)
            .data_i16(&levels)]),
    ));
    seeds.push((
        "fits/naxis0-primary".into(),
        common::file(&[Hdu::primary().card("BITPIX", "8").card("NAXIS", "0")]),
    ));
    seeds.push((
        "fits/nonconforming".into(),
        common::file(&[Hdu::nonconforming_primary()]),
    ));
    // The data-unit extent that ends past `u64` — `Malformed` on the second advance, and an
    // `attempt to add with overflow` panic before `consume_current` used `checked_add`.
    //
    // Seeded even though the fuzzer cannot rediscover it: the trigger is one specific 19-digit
    // decimal constant, and no mutation of a neighbouring value reaches it. What the seed buys
    // is the *shape* — a declined position followed by a second advance over an enormous
    // declared extent — which a mutator can vary in the digits it does reach. The assertion
    // that actually holds the fix is
    // `fits_declines::a_data_unit_whose_extent_ends_past_u64_is_malformed_on_the_next_advance`.
    seeds.push((
        "fits/data-unit-extent-past-u64".into(),
        common::file(&[Hdu::primary()
            .card("BITPIX", "16")
            .card("NAXIS", "1")
            .card("NAXIS1", "9223372036854774368")]),
    ));

    let data = repeating_u16(64);
    seeds.push((
        "xisf/uncompressed".into(),
        Unit::new().image_u16(8, 8, 1, &data).build(),
    ));
    // **A block split into subblocks**, which no other seed carries. §10.6 restarts the codec
    // at every boundary, so the whole per-subblock cost — the input window and the codec state
    // both — runs a number of times bounded by `Subblock count` rather than by the input, and a
    // mutator that never assembles a `subblocks` attribute never reaches that loop at all.
    // Eight is small on purpose: what the seed buys is the shape, a `c,u` pair list agreeing
    // with the stored bytes behind it, and the count is the part a mutator can raise.
    let (subblocked, list) = {
        let raw = le_u16(&repeating_u16(64));
        let mut stored = Vec::new();
        let mut list = Vec::new();
        for chunk in raw.chunks(raw.len() / 8) {
            let c = zlib(chunk);
            list.push(format!("{},{}", c.len(), chunk.len()));
            stored.extend_from_slice(&c);
        }
        (stored, list.join(":"))
    };
    seeds.push((
        "xisf/subblocked-zlib".into(),
        Unit::new()
            .attached(
                &format!(
                    r#"<Image geometry="8:8:1" sampleFormat="UInt16" compression="zlib:128" subblocks="{list}" {{loc}}/>"#
                ),
                subblocked,
            )
            .build(),
    ));
    seeds.push((
        "xisf/header-only".into(),
        Unit::new().image_u16(8, 8, 1, &data).build_header_only(),
    ));
    seeds.push((
        "xisf/no-image".into(),
        Unit::new().xml("<Metadata/>").build(),
    ));
    seeds.push((
        "xisf/short-preamble".into(),
        raw_unit(b"XISF0100", 0, "", &[]),
    ));
    seeds.push((
        "xisf/bad-signature".into(),
        raw_unit(
            b"XISF0200",
            65,
            "<?xml version=\"1.0\"?><xisf version=\"1.0\"/>",
            &[],
        ),
    ));

    // **The multiplication shapes**, small. Every allocation defect this crate has had was of
    // one form — header text multiplied by a count bounded by a cap rather than by the input —
    // and all four were built by hand from reading the code,
    // while 52 million fuzzed cases found none. They need structure a mutator reaches slowly:
    // a `uid`, a matching `ref`, and then many references. Seeded at a small count, the
    // mutator can raise the count; unseeded it never assembles the shape at all.
    let big = "v".repeat(512);
    seeds.push((
        "xisf/keyword-by-many-references".into(),
        Unit::new()
            .xml(&format!(
                r#"<FITSKeyword uid="k" name="N" value="{big}" comment=""/><Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data>{}</Image>"#,
                r#"<Reference ref="k"/>"#.repeat(24)
            ))
            .build(),
    ));
    seeds.push((
        "xisf/image-by-many-references".into(),
        Unit::new()
            .xml(&format!(
                r#"<Image uid="im" id="{big}" geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data></Image>{}"#,
                r#"<Reference ref="im"/>"#.repeat(24)
            ))
            .build(),
    ));
    seeds.push((
        "xisf/root-metadata-and-images".into(),
        Unit::new()
            .xml(&format!(
                r#"<Metadata><Property id="X" type="String" value="{big}"/></Metadata>{}"#,
                r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data><Property id="own" type="String" value="o"/></Image>"#.repeat(8)
            ))
            .build(),
    ));
    seeds.push((
        "xisf/continue-chain".into(),
        Unit::new()
            .xml(&format!(
                r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data><FITSKeyword name="LONGSTR" value="'aaaa&amp;'" comment=""/>{}</Image>"#,
                r#"<FITSKeyword name="CONTINUE" value="'aaaa&amp;'" comment=""/>"#.repeat(24)
            ))
            .build(),
    ));

    // **The two-multiplier shapes**, small. The four above each grow one count; these are
    // products of two, which is what every instance of the class has actually been, and 28.5
    // million executions found neither. Both need structure a mutator cannot assemble from
    // nothing — several distinct images agreeing on the same `ref`, or a primary header
    // followed by well-formed extension headers on block boundaries — so they are seeded with
    // both counts small and the mutator left to raise either.
    // The images carry a keyword of their own, which is what makes them **distinct**. Eight
    // repetitions of one image is the shape a memo hits however it is keyed, so the seed as
    // repetition seeded the mutator *away* from the defect: keyed on the whole image's record
    // sequence, the assembly was shared across identical images and rebuilt per image the
    // moment one keyword differed — 982x the input at 256 images, and nothing at all at 8
    // identical ones.
    let images: String = (0..8)
        .map(|i| {
            format!(
                r#"<Image geometry="1:1:1" sampleFormat="UInt16" location="embedded"><Data encoding="base64">AAA=</Data><FITSKeyword name="U{i:05}" value="1" comment=""/><Reference ref="k0"/><Reference ref="k1"/></Image>"#
            )
        })
        .collect();
    seeds.push((
        "xisf/shared-continue-chain-across-images".into(),
        Unit::new()
            .xml(&format!(
                r#"<FITSKeyword uid="k0" name="LONGSTR" value="'{big}&amp;'" comment=""/><FITSKeyword uid="k1" name="CONTINUE" value="'{big}'" comment=""/>{images}"#
            ))
            .build(),
    ));
    {
        let mut primary = Hdu::primary().card("BITPIX", "8").card("NAXIS", "0");
        for i in 0..24 {
            primary = primary.commentary("HISTORY", &format!("card {i:06}"));
        }
        let mut hdus = vec![primary];
        for _ in 0..8 {
            hdus.push(
                Hdu::extension("IMAGE")
                    .card("BITPIX", "8")
                    .card("NAXIS", "2")
                    .card("NAXIS1", "0")
                    .card("NAXIS2", "1")
                    .card("PCOUNT", "0")
                    .card("GCOUNT", "1"),
            );
        }
        seeds.push((
            "fits/primary-cards-and-extensions".into(),
            common::file(&hdus),
        ));
    }
    {
        // The other FITS product: an inherited `ROWORDER` whose value a `CONTINUE` chain
        // assembles, applied by every extension. The continuation cards are raw because a
        // conforming `CONTINUE` record carries no value indicator.
        let text = "A".repeat(60);
        let mut primary = Hdu::primary()
            .card("BITPIX", "8")
            .card("NAXIS", "0")
            .card("ROWORDER", &format!("'{text}&'"));
        for _ in 0..8 {
            primary = primary.raw(format!("CONTINUE  '{text}&'").as_bytes());
        }
        primary = primary.raw(b"CONTINUE  'z'");
        let mut hdus = vec![primary];
        for _ in 0..8 {
            hdus.push(
                Hdu::extension("IMAGE")
                    .card("BITPIX", "8")
                    .card("NAXIS", "2")
                    .card("NAXIS1", "0")
                    .card("NAXIS2", "1")
                    .card("PCOUNT", "0")
                    .card("GCOUNT", "1")
                    .card("INHERIT", "T"),
            );
        }
        seeds.push(("fits/inherited-roworder-chain".into(), common::file(&hdus)));
    }

    // Degenerate inputs the sniffer must survive.
    seeds.push(("raw/empty".into(), Vec::new()));
    seeds.push(("raw/short".into(), b"XIS".to_vec()));
    seeds.push(("raw/zeros".into(), vec![0u8; 64]));
    seeds
}

/// Both seed sources, replayed in **one** `#[test]` — see the module header: the counting
/// allocator is process-wide, so a second test allocating on another thread lands in this
/// one's counter.
#[test]
fn every_seed_replays_within_the_fuzz_oracles_bound() {
    for (name, bytes) in synthetic_seeds() {
        replay(&bytes);
        println!("replayed {name} ({} bytes)", bytes.len());
    }
    committed_corpus_and_crash_artifacts();
}

/// Write the synthetic seeds into `fuzz/corpus/<target>/`, which is what the committed corpus
/// is: **seeds, not the fuzzer's exploration output.**
///
/// It matters far more than it looks, and the difference was measured rather than assumed.
/// Two 20-second runs of `decode_any`, one from an empty corpus and one from these seeds:
///
/// ```text
/// bare:    #6050473  cov: 63    ft: 80     corp: 7/43b
/// seeded:  #1926518  cov: 2813  ft: 8144   corp: 1544/2074Kb
/// ```
///
/// **Forty-four times the coverage.** Bare, six million executions are almost all turned away
/// at format sniffing and never reach a decoder at all — the fuzzer is testing the signature
/// check. Starting from a file that parses is what puts the mutator inside the parser, which
/// is the only place the guards it exists to exercise actually live.
///
/// `#[ignore]`d because it writes to the repository; run it deliberately:
///
/// ```text
/// cargo test --all-features --test fuzz_replay -- --ignored seed_the_fuzz_corpus
/// ```
///
/// libFuzzer also writes its own discoveries into these directories during a run. Those are
/// **not** committed — only these seeds and, per the design, any crash artifact once its bug
/// is fixed.
#[test]
#[ignore = "writes fuzz/corpus; run deliberately"]
fn seed_the_fuzz_corpus() {
    const TARGETS: [&str; 6] = [
        "fits_header",
        "xisf_header",
        "xisf_block",
        "decode_any",
        "decode_sequential",
        "decode_alloc",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus");
    let seeds = synthetic_seeds();
    for target in TARGETS {
        let dir = root.join(target);
        std::fs::create_dir_all(&dir).expect("create the corpus directory");
        for (name, bytes) in &seeds {
            let seed: &[u8] = match target {
                // `fits_header` and `xisf_header` prepend the format's signature themselves,
                // so their corpora hold the seed with that prefix taken off — written whole,
                // every seed here would be parsed doubled. See `wrap_as_fits_header`.
                "fits_header" => strip_fits_header(bytes),
                "xisf_header" => strip_xisf_header(bytes),
                // `xisf_block` takes a (header, body) tuple through `Arbitrary`, so a bare
                // unit is not a valid input for it; its seeds are the tuple encoding, which
                // libFuzzer derives from these bytes well enough to start from.
                _ => bytes,
            };
            let file = dir.join(name.replace('/', "-"));
            std::fs::write(&file, seed).expect("write a seed");
        }
    }
    println!(
        "seeded {} files per target across {} targets",
        seeds.len(),
        TARGETS.len()
    );
}

/// Every file under `fuzz/corpus/` and `fuzz/artifacts/`, which is the committed seeds plus
/// whatever a local fuzzing run has left beside them.
///
/// Each file is replayed as **its own target** would see it: a file under a directory named
/// for a target that wraps its input is wrapped the same way, so the shape the fuzzer
/// actually parses is the shape this lane asserts on.
///
/// **Deduplicated by content**, because the six corpora largely hold the same seeds and
/// replaying each of them once per target is six passes over one input. The alternative —
/// one shared directory the raw-bytes targets are pointed at with an extra command-line
/// argument — saves the duplicated bytes on disk as well, and is rejected for it: a corpus
/// reached only by remembering an argument is a corpus that silently stops feeding the
/// fuzzer, which is the same class of defect the wrapping above exists to rule out. The set
/// holds what is handed to [`replay`], after wrapping, so a `fits_header` seed collapses
/// onto the `decode_any` copy it is stripped from while a wrapped XISF header region stays
/// distinct from the full unit it comes from.
fn committed_corpus_and_crash_artifacts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz");
    let mut replayed = 0usize;
    let mut duplicates = 0usize;
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for dir in ["corpus", "artifacts"] {
        let base = root.join(dir);
        if !base.is_dir() {
            continue;
        }
        let mut stack = vec![base.clone()];
        while let Some(path) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let Ok(bytes) = std::fs::read(&p) else {
                    continue;
                };
                let input = match target_directory(&base, &p) {
                    "fits_header" => wrap_as_fits_header(&bytes),
                    "xisf_header" => wrap_as_xisf_header(&bytes),
                    _ => bytes,
                };
                if seen.contains(&input) {
                    duplicates += 1;
                    continue;
                }
                replay(&input);
                seen.insert(input);
                replayed += 1;
            }
        }
    }
    println!(
        "replayed {replayed} distinct corpus and artifact files, {duplicates} duplicates skipped"
    );
}

/// The target a corpus or artifact file belongs to: the first path component under
/// `fuzz/corpus/` or `fuzz/artifacts/`. A file sitting directly in either directory belongs
/// to no target and is replayed raw.
fn target_directory<'a>(base: &std::path::Path, file: &'a std::path::Path) -> &'a str {
    file.strip_prefix(base)
        .ok()
        .filter(|relative| relative.components().count() > 1)
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or("")
}
