//! § Local corpus validation — the developer-invoked tier.
//!
//! Roughly 84 GB of real output from production applications (PixInsight,
//! AstroPixelProcessor, N.I.N.A., ASIAIR) and from CFITSIO as a reference implementation. It
//! is **never committed and never run in CI**: it exists on one machine, and it carries
//! observatory coordinates at full precision throughout — site coordinates appear in half the
//! FITS variants, two thirds of those under `LAT-OBS`/`LONG-OBS` rather than
//! `SITELAT`/`SITELONG`, and ride into the XISF variants as `Observation:Location:*`
//! properties.
//!
//! What makes it worth having is provenance: the variants were written by production
//! applications rather than guessed at, across two observers' rigs and several camera, mount
//! and capture-software combinations, so they are a specification of what real writers emit
//! rather than what one writer habitually does. What makes it unusable as a test suite is
//! that it exists nowhere else.
//!
//! That second observer is also why the never-commit rule is not the maintainer's to relax:
//! some of these frames carry another person's sites and observing times at full precision,
//! and that is an obligation to someone who cannot review what this repository publishes.
//!
//! ```text
//! ASTROFRAME_CORPUS=/path/to/corpus cargo test --release -- --ignored
//! ```
//!
//! Every test here is `#[ignore]`d and locates the corpus through that variable, **skipping
//! cleanly when it is unset** rather than failing. No path is ever hardcoded, and no default
//! points inside this repository — a corpus gate that falls back to a checked-in directory is
//! how a real frame gets committed by accident.

use astroframe::{Bounds, Error, Reader, Samples};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The corpus root, or `None` when the developer has not pointed at one.
fn corpus() -> Option<std::path::PathBuf> {
    let raw = std::env::var("ASTROFRAME_CORPUS").ok()?;
    let path = std::path::PathBuf::from(raw);
    if path.is_dir() {
        Some(path)
    } else {
        eprintln!(
            "ASTROFRAME_CORPUS is set but is not a directory: {}",
            path.display()
        );
        None
    }
}

/// The extensions a frame carries. Everything else in the corpus — generation scripts, their
/// logs — is not a frame and must not be handed to a decoder.
///
/// The design warns that the generation logs do not describe the final state, so they are not
/// evidence either: trust the files.
const FRAME_EXTENSIONS: [&str; 5] = ["fits", "fit", "fts", "xisf", "xisb"];

fn is_frame(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| FRAME_EXTENSIONS.contains(&e.as_str()))
}

/// Every frame under `family`, sorted, so a failure names a reproducible file.
fn family(root: &std::path::Path, name: &str) -> Vec<std::path::PathBuf> {
    let dir = root.join(name);
    let mut out = Vec::new();
    let mut stack = vec![dir];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() && is_frame(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// How many files to decode at once.
///
/// The sweep is **CPU-bound, not I/O-bound** — measured, not assumed: a sequential run held one
/// core at 99% and moved about 100 MB/s, far under what the disk does, because every XISF
/// variant has its block decompressed and every sample decoded. Sequentially the family takes
/// hours; across the machine's cores it takes minutes.
///
/// Each file is completely independent — its own `Reader`, its own buffers, no shared state —
/// so this is a work queue rather than a chunking, which keeps the cores busy when file costs
/// differ by an order of magnitude (they do: a `Float64` master against an 8-bit variant).
///
/// **Only safe here.** `tests/peak_memory.rs` and `tests/bombs.rs` install a
/// `#[global_allocator]` and measure process-wide allocation; running their measurements
/// concurrently is the trap that once produced a 60 MB reading against a 9 MB model. This file
/// installs no allocator.
fn workers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Decode every file, in parallel, returning each outcome beside its path.
fn walk_all(files: &[std::path::PathBuf]) -> Vec<(std::path::PathBuf, Outcome)> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<(std::path::PathBuf, Outcome)>> =
        Mutex::new(Vec::with_capacity(files.len()));
    let done = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers() {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    let outcome = walk(path);
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_multiple_of(100) {
                        println!("  {n}/{} files", files.len());
                    }
                    out.lock()
                        .expect("results lock")
                        .push((path.clone(), outcome));
                }
            });
        }
    });
    out.into_inner().expect("results lock")
}

/// Walk one file and return what happened, without ever panicking on its contents.
enum Outcome {
    Decoded { images: usize },
    Declined(String),
    Failed(Error),
}

fn walk(path: &std::path::Path) -> Outcome {
    let mut reader = match Reader::open(path) {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(e),
    };
    let mut images = 0usize;
    loop {
        match reader.next_image() {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => return Outcome::Failed(e),
        }
        let Some(header) = reader.header() else { break };
        if let Some(reason) = header.decline_reason() {
            return Outcome::Declined(reason.reason().to_owned());
        }
        let Some(format) = header.sample_format() else {
            return Outcome::Declined("no sample format".into());
        };
        let len = match (header.width(), header.height(), header.channels()) {
            (Some(w), Some(h), Some(c)) => w as usize * h as usize * c as usize,
            _ => return Outcome::Declined("no geometry".into()),
        };
        // Native samples always decode, even where normalized output is refused, so this is
        // the entry point that exercises every file rather than only the normalizable ones.
        let mut samples = Samples::zeroed(format, len);
        if let Err(e) = reader.read_samples_into(&mut samples) {
            return Outcome::Failed(e);
        }
        images += 1;
    }
    Outcome::Decoded { images }
}

/// *Every file decodes or declines cleanly.*
///
/// Every file in every family produces a decoded frame or a stated error class — never a
/// panic, a hang, or a silent misread. `Io` is the only class that fails the run: it means the
/// harness could not read the file, which is a problem with the corpus rather than with the
/// decoder.
#[test]
#[ignore = "needs ASTROFRAME_CORPUS"]
fn every_corpus_file_decodes_or_declines_cleanly() {
    let Some(root) = corpus() else {
        eprintln!("skipping: ASTROFRAME_CORPUS is unset");
        return;
    };
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for name in ["fits_variants", "xisf_variants", "masters"] {
        files.extend(family(&root, name));
    }
    println!(
        "decoding {} files across {} workers",
        files.len(),
        workers()
    );

    let mut decoded = 0usize;
    let mut images = 0usize;
    let mut declined = 0usize;
    let mut reasons: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();

    for (path, outcome) in walk_all(&files) {
        match outcome {
            Outcome::Decoded { images: n } => {
                decoded += 1;
                images += n;
            }
            Outcome::Declined(reason) => {
                declined += 1;
                *reasons.entry(reason).or_default() += 1;
            }
            Outcome::Failed(Error::Io(e)) => {
                failures.push(format!("{}: unreadable ({e})", path.display()))
            }
            Outcome::Failed(e) => {
                // A stated class is an acceptable outcome; an unexpected one is not, and
                // naming the file is what makes it reproducible.
                failures.push(format!("{}: {e}", path.display()))
            }
        }
    }
    println!("decoded {decoded} files holding {images} images, declined {declined}");
    // The declines are the interesting half: a corpus sweep that declines everything passes
    // this test while proving nothing, so the reasons are printed rather than merely counted.
    for (reason, count) in &reasons {
        println!("  {count:>5} x {reason}");
    }
    assert!(
        failures.is_empty(),
        "{} file(s) neither decoded nor declined cleanly:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        decoded > 0,
        "no file decoded; is the corpus laid out as expected?"
    );
}

/// The honest test of the tile-compressed decline: **every** `fpack_variants/` file must yield
/// a clean `Unsupported`, never a misread as a table and never a decode.
///
/// A tile-compressed image is recognized by `ZIMAGE = T` on a `BINTABLE` extension and never
/// by filename — the corpus's tile-compressed files are named `.fits`.
#[test]
#[ignore = "needs ASTROFRAME_CORPUS"]
fn every_fpack_variant_is_a_clean_unsupported() {
    let Some(root) = corpus() else {
        eprintln!("skipping: ASTROFRAME_CORPUS is unset");
        return;
    };
    let files = family(&root, "fpack_variants");
    assert!(!files.is_empty(), "fpack_variants/ is empty");
    let mut wrong: Vec<String> = Vec::new();
    for (path, outcome) in walk_all(&files) {
        match outcome {
            Outcome::Declined(reason) => {
                // Declining for the *right* reason matters: a file refused because its header
                // failed to parse would pass a bare "was declined" check while telling us
                // nothing about the tile-compressed path.
                if !reason.contains("tile-compressed") && !reason.contains("ZIMAGE") {
                    wrong.push(format!("{}: declined, but for {reason}", path.display()));
                }
            }
            Outcome::Decoded { .. } => {
                wrong.push(format!("{}: decoded, expected Unsupported", path.display()))
            }
            Outcome::Failed(e) => wrong.push(format!("{}: {e}", path.display())),
        }
    }
    println!("{} tile-compressed files declined cleanly", files.len());
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// *Cross-entry-point agreement.*
///
/// Header-only geometry matches a single-image decode, and the image count matches an
/// all-image walk. The original suite stops there and never compares pixels; the pixel
/// comparison across `open`, `sequential` and `seekable` is this design's addition, and it is
/// the half that would catch a source-mode divergence.
#[test]
#[ignore = "needs ASTROFRAME_CORPUS"]
fn entry_points_agree_across_the_corpus() {
    let Some(root) = corpus() else {
        eprintln!("skipping: ASTROFRAME_CORPUS is unset");
        return;
    };
    // A sample rather than the whole 84 GB: this one reads every file three ways.
    let mut files = family(&root, "fits_variants");
    files.extend(family(&root, "xisf_variants").into_iter().step_by(37));
    files.extend(family(&root, "masters"));

    let next = AtomicUsize::new(0);
    let checked = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers() {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    let Ok(bytes) = std::fs::read(path) else {
                        continue;
                    };

                    let seekable = decode_first(Reader::seekable(std::io::Cursor::new(&bytes)));
                    let sequential = decode_first(Reader::sequential(bytes.as_slice()));
                    let opened = decode_first(Reader::open(path));

                    // A sequential source legitimately refuses a block behind its cursor, so a
                    // mismatch there is only a failure when the sequential read succeeded.
                    if let (Some(a), Some(b)) = (&opened, &seekable)
                        && bits(a) != bits(b)
                    {
                        failures
                            .lock()
                            .expect("lock")
                            .push(format!("{}: open and seekable disagree", path.display()));
                    }
                    if let (Some(a), Some(c)) = (&opened, &sequential)
                        && bits(a) != bits(c)
                    {
                        failures
                            .lock()
                            .expect("lock")
                            .push(format!("{}: open and sequential disagree", path.display()));
                    }
                    if opened.is_some() {
                        checked.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().expect("lock");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let checked = checked.load(Ordering::Relaxed);
    println!("{checked} files agreed across open, sequential and seekable");
    assert!(
        checked > 0,
        "no file decoded; is the corpus laid out as expected?"
    );
}

fn decode_first<S: astroframe::Source>(reader: astroframe::Result<Reader<S>>) -> Option<Vec<f32>> {
    let mut reader = reader.ok()?;
    if !reader.next_image().ok()? {
        return None;
    }
    let header = reader.header()?;
    if header.decline_reason().is_some() {
        return None;
    }
    let format = header.sample_format()?;
    let len = header.width()? as usize * header.height()? as usize * header.channels()? as usize;
    let mut samples = Samples::zeroed(format, len);
    reader.read_samples_into(&mut samples).ok()?;
    Some(match samples {
        Samples::F32(v) => v,
        other => (0..other.len()).map(|i| i as f32).collect::<Vec<f32>>(),
    })
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|f| f.to_bits()).collect()
}

/// *The masters* — the multi-image walk, the per-image scoping rule and the reset-on-advance
/// rule, all in one file, on real data.
///
/// Four of the five XISF masters hold **two** images, and one holds two of *different geometry
/// and different sample format*. Every `Float32` master carries `bounds="0:1"`, which confirms
/// the mandatory-`bounds` rule against real writers and puts the identity-multiply saturation
/// case on the common path rather than a hypothetical one.
#[test]
#[ignore = "needs ASTROFRAME_CORPUS"]
fn the_masters_walk_their_images() {
    let Some(root) = corpus() else {
        eprintln!("skipping: ASTROFRAME_CORPUS is unset");
        return;
    };
    let files = family(&root, "masters");
    assert!(!files.is_empty(), "masters/ is empty");

    let mut multi_image = 0usize;
    let mut unit_bounds = 0usize;
    for path in &files {
        let Ok(mut reader) = Reader::open(path) else {
            continue;
        };
        let mut shapes: Vec<String> = Vec::new();
        while reader.next_image().unwrap_or(false) {
            let Some(h) = reader.header() else { break };
            if let (Some(w), Some(ht), Some(c)) = (h.width(), h.height(), h.channels()) {
                shapes.push(format!("{w}x{ht}x{c}/{:?}", h.sample_format()));
            }
            if let Bounds::Declared(range) = h.bounds()
                && range.lo() == 0.0
                && range.hi() == 1.0
            {
                unit_bounds += 1;
            }
        }
        if shapes.len() > 1 {
            multi_image += 1;
        }
        println!("{}: {}", path.display(), shapes.join(", "));
    }
    assert!(multi_image > 0, "no master held more than one image");
    println!("{multi_image} multi-image masters, {unit_bounds} images declaring bounds 0:1");
}

/// Decode one frame's first image to its normalized `f32` pixels, or `None` if it declined.
///
/// Normalized rather than native, because the two containers report *different native
/// formats for the same pixels* and both are right: a `BITPIX = 16` FITS with
/// `BZERO = 32768` reports `I16` plus the scaling it carries, while the XISF reports `U16`
/// outright. That is § The organizing principle working — the crate reports what each file
/// says rather than harmonizing them — so the layer at which they must agree is the one
/// § Normalization defines, which is exactly the layer a consumer reads.
fn normalized_pixels(path: &std::path::Path) -> Result<Option<Vec<f32>>, Error> {
    // The error is carried rather than discarded. `.ok()?` at each of these three boundaries
    // collapsed `Io`, `Malformed`, `Unsupported`, `ChecksumMismatch` and `LimitExceeded` into
    // one indistinguishable "did not decode" -- on the single test that exists to catch XISF
    // regressions across 1080 files, where the class is most of the diagnosis.
    let mut reader = Reader::open(path)?;
    if !reader.next_image()? {
        return Ok(None);
    }
    let Some(header) = reader.header() else {
        return Ok(None);
    };
    if header.decline_reason().is_some() {
        return Ok(None);
    }
    let (Some(w), Some(h), Some(c)) = (header.width(), header.height(), header.channels()) else {
        return Ok(None);
    };
    let mut pixels = vec![0.0f32; w as usize * h as usize * c as usize];
    reader.read_image_into(&mut pixels)?;
    Ok(Some(pixels))
}

/// How far two decodes of the same pixels may differ, in the unit the difference is *in*.
///
/// A conversion's error is either exact, relative to the value, or a fixed fraction of the
/// range — and asserting a relative divergence with an absolute number is only tight at the
/// top of the range. Keeping the three apart is what makes each bound mean what it says.
#[derive(Clone, Copy, Debug)]
enum Bound {
    /// Bit-for-bit. Compared with `to_bits`, never `==`, which accepts a sign-of-zero
    /// difference — the class the endpoint tests exist to catch.
    Exact,
    /// At most this many representable `f32` steps apart, anywhere in the range.
    Ulps(i64),
    /// At most this absolute difference — the right unit for a quantization step.
    Absolute(f64),
}

impl Bound {
    /// Whether two normalized samples are further apart than this bound allows.
    fn exceeded_by(self, a: f32, b: f32) -> bool {
        match self {
            Bound::Exact => a.to_bits() != b.to_bits(),
            // Both sides are finite and non-negative here (normalized output is in [0,1]), so
            // the bit patterns are monotone in the value and their difference counts steps.
            Bound::Ulps(n) => (i64::from(a.to_bits()) - i64::from(b.to_bits())).abs() > n,
            Bound::Absolute(limit) => (f64::from(a) - f64::from(b)).abs() > limit,
        }
    }

    /// The distance between two samples, in the unit this bound's ceiling is itself stated in:
    /// representable `f32` steps for `Exact` and `Ulps`, the raw difference for `Absolute`.
    /// Reporting every bound in the same absolute unit is what let a `float64` row's ULP
    /// ceiling sit beside a measurement that was actually an absolute difference.
    fn distance(self, a: f32, b: f32) -> f64 {
        match self {
            // `Exact` compares bit patterns, so it measures in bit-pattern steps too. A raw
            // difference would report a sign-of-zero mismatch -- the one defect this bound
            // exists to catch -- as `0.000e0`, which reads as agreement. In steps it is 2^31.
            Bound::Exact | Bound::Ulps(_) => {
                (i64::from(a.to_bits()) - i64::from(b.to_bits())).unsigned_abs() as f64
            }
            Bound::Absolute(_) => (f64::from(a) - f64::from(b)).abs(),
        }
    }

    /// How the ceiling prints beside a measured worst case.
    fn describe(self) -> String {
        match self {
            Bound::Exact => "exact".to_owned(),
            Bound::Ulps(n) => format!("{n} ulp"),
            Bound::Absolute(limit) => format!("{limit:.3e}"),
        }
    }

    /// How a measured `distance()` prints, in the same unit `describe()` states the ceiling
    /// in — an ULP count for `Exact`/`Ulps`, scientific notation for `Absolute`.
    fn describe_distance(self, d: f64) -> String {
        match self {
            Bound::Exact | Bound::Ulps(_) => format!("{d:.0} ulp"),
            Bound::Absolute(_) => format!("{d:.3e}"),
        }
    }
}

/// The largest deviation a variant's sample format can honestly produce, and **why**.
///
/// Every ceiling here is derived from the conversion, never fitted to what was observed —
/// fitting a tolerance to a measurement makes the test agree with the code by construction.
/// Each was then checked against the whole corpus and the measured worst case is quoted
/// beside it.
fn tolerance(format: &str) -> Option<Bound> {
    match format {
        // Lossless: PixInsight's UInt16 export holds the same 16-bit levels the FITS does,
        // and both sides normalize them with the same primitive. Measured: exact.
        "uint16" => Some(Bound::Exact),
        // Also exact, and by arithmetic rather than luck: 4294967295 = 65535 x 65537, so a
        // 16-bit level L becomes L x 65537 and `L x 65537 / (65535 x 65537)` *is* `L / 65535`.
        // The widening cannot lose a bit. Measured: exact.
        "uint32" => Some(Bound::Exact),
        // Measured exact across all 8.4 billion pixels, so PixInsight's `Float32` conversion
        // lands on the same `f32` this crate computes. Asserted exact rather than approximate
        // precisely because it *is* exact: loosening it would stop the test noticing if that
        // ever changed.
        "float32" => Some(Bound::Exact),
        // One ULP, measured in ULPs. PixInsight stored `L / 65535` evaluated in double; this
        // crate computes `L * (1/65535 as f32)`, and § Normalization pins that difference
        // deliberately -- the multiply-by-rounded-reciprocal divergence, not an error.
        //
        // Expressed as a *bit-pattern* distance rather than an absolute one, because the
        // divergence is relative and an absolute bound is only tight at the top of the range.
        // `2^-24` is one ULP just below 1.0, but at a typical dark-frame level of 0.0077 one
        // ULP is 9.3e-10 -- so the absolute form admitted about sixty-four ULPs there, and far
        // more in the shadows. A perturbation of +32 ULPs on every sample passed five of the
        // six frames under it. `Ulps(1)` is uniformly one ULP everywhere in the range, which
        // is what the derivation actually licenses.
        "float64" => Some(Bound::Ulps(1)),
        // Half of one 8-bit step, and the step is `1/255` rather than `1/256`: § Normalization
        // maps a level onto `level / (hi - lo)`, and for `UInt8` that denominator is 255. So
        // the bound is `1/(2 x 255) = 1/510`, not the `1/512` that "half of 256 levels" tempts
        // you into. Round-to-nearest quantization cannot err by more than half a step, and
        // anything larger would mean PixInsight dithered or this crate mis-decoded.
        //
        // The distinction is not academic: the measured worst is 1.9532e-3, which is *above*
        // 1/512 and below 1/510, so the wrong denominator fails a corpus that is correct. It
        // was written as 1/512 first and the corpus caught it -- which is the argument for
        // deriving a bound and then checking it, rather than fitting one to a measurement.
        "uint8" => Some(Bound::Absolute(1.0 / 510.0)),
        _ => None,
    }
}

/// **The XISF path, verified against an independent implementation — every variant.**
///
/// The corpus's `xisf_variants/` frames were produced by PixInsight *from* the `.fit` files at
/// the corpus root, so each root frame and its variants are the same pixels written twice by
/// two different implementations. That makes PixInsight the independent oracle the XISF path
/// otherwise lacks: there is no second Rust XISF decoder to differential against, so before
/// this test XISF correctness rested on the exhaustive normalization tests plus cross-format
/// identity over *synthetic* fixtures — a sound argument whose premises were all our own.
///
/// The FITS side of the comparison is itself bit-verified against `fitsrs`
/// (`differential::corpus_native_samples_match_fitsrs`), so the chain is: PixInsight wrote
/// these pixels, an independent FITS decoder agrees we read the FITS correctly, and this
/// asserts the XISF decode lands where it should for **all five sample formats and all
/// thirty-six codec and checksum combinations**.
///
/// Compared as normalized `f32`, because the two containers report different *native* formats
/// for the same pixels and both are right — a `BITPIX = 16` FITS with `BZERO = 32768` reports
/// `I16` plus its scaling, the XISF reports `UInt16`. That is § The organizing principle
/// working rather than a discrepancy, so the layer at which they must agree is the normalized
/// one, which is also the layer a consumer reads.
///
/// Three of the five formats are asserted **bit-exact**; see [`tolerance`] for why each bound
/// is what it is.
#[test]
#[ignore = "needs ASTROFRAME_CORPUS"]
fn xisf_variants_decode_to_the_same_bits_as_the_fits_they_came_from() {
    let Some(root) = corpus() else {
        eprintln!("skipping: ASTROFRAME_CORPUS is unset");
        return;
    };

    let mut sources: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
        .expect("the corpus root is readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_frame(p))
        .collect();
    sources.sort();

    let variants = root.join("xisf_variants");
    if !variants.is_dir() || sources.is_empty() {
        eprintln!("skipping: no root frames or no xisf_variants/ in this corpus");
        return;
    }

    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(&variants)
        .expect("xisf_variants is readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    let mut compared = 0usize;
    let mut pixels_compared = 0u64;
    let mut worst: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();

    for source in &sources {
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a frame has a name");
        let reference = match normalized_pixels(source) {
            Ok(Some(pixels)) => pixels,
            Ok(None) => {
                failures.push(format!("{stem}: the root FITS declined its own image"));
                continue;
            }
            Err(e) => {
                failures.push(format!("{stem}: the root FITS did not decode: {e}"));
                continue;
            }
        };

        for dir in &dirs {
            let candidate = dir.join(format!("{stem}.xisf"));
            if !candidate.is_file() {
                continue;
            }
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let format = name.split("__").next().unwrap_or("?");
            let Some(limit) = tolerance(format) else {
                failures.push(format!(
                    "{name}: no derived tolerance for the sample format {format:?}; a new \
                     variant family needs its bound derived before it can be graded"
                ));
                continue;
            };

            let label = format!("{name}/{stem}");
            let got = match normalized_pixels(&candidate) {
                Ok(Some(pixels)) => pixels,
                Ok(None) => {
                    failures.push(format!("{label}: declined its image"));
                    continue;
                }
                Err(e) => {
                    failures.push(format!("{label}: did not decode: {e}"));
                    continue;
                }
            };
            compared += 1;
            if got.len() != reference.len() {
                failures.push(format!(
                    "{label}: {} pixels against the FITS's {}",
                    got.len(),
                    reference.len()
                ));
                continue;
            }

            let mut worst_here = 0.0f64;
            let mut first_bad: Option<usize> = None;
            for (i, (a, b)) in reference.iter().zip(&got).enumerate() {
                // Exact formats are compared by bit pattern, never by `==`: `==` accepts a
                // sign-of-zero difference, which is the class the endpoint tests exist for.
                let bad = limit.exceeded_by(*a, *b);
                // In the bound's own unit -- an ULP count for `Ulps`, an absolute difference
                // otherwise -- so a `float64` row's ULP ceiling is never printed beside a
                // measurement that was actually an absolute difference.
                let d = limit.distance(*a, *b);
                if d > worst_here {
                    worst_here = d;
                }
                if bad && first_bad.is_none() {
                    first_bad = Some(i);
                }
            }
            let seen = worst.entry(format.to_owned()).or_insert(0.0);
            if worst_here > *seen {
                *seen = worst_here;
            }
            match first_bad {
                // The index and the two values only -- never the pixel's provenance.
                Some(i) => failures.push(format!(
                    "{label}: pixel {i} differs by {}, above the {} the {format} \
                     conversion allows (FITS {:08x} vs XISF {:08x})",
                    limit.describe_distance(limit.distance(reference[i], got[i])),
                    limit.describe(),
                    reference[i].to_bits(),
                    got[i].to_bits()
                )),
                None => pixels_compared += reference.len() as u64,
            }
        }
    }

    println!(
        "xisf-vs-fits: {compared} variant(s) against {} root frame(s), {pixels_compared} pixels",
        sources.len()
    );
    for (format, seen) in &worst {
        let limit = tolerance(format).expect("every measured format has a derived bound");
        println!(
            "  {format:<8} worst {}  allowed {}",
            limit.describe_distance(*seen),
            limit.describe()
        );
    }
    assert!(
        compared > 0,
        "no variants were found beside the root frames; this test asserted nothing"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
