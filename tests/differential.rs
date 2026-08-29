//! Criterion *Differential FITS check*: this crate's **native** samples against `fitsrs`'s,
//! for every pixel.
//!
//! Comparing *native* samples is the point rather than an accident of convenience. Any FITS
//! crate that applied `BSCALE`/`BZERO` in `f64` would reintroduce the exact 1-ULP defect the
//! design exists to prevent, so the comparison is taken **upstream of scaling**, where the two
//! implementations must agree bit for bit regardless of what either does afterwards. `fitsrs`
//! 0.4.1 in fact applies that scaling on no path at all, so the property is unconditional and
//! needs no "give me unscaled values" entry point on either side.
//!
//! Two tiers, per § Acceptance Criteria. The fixture tier runs in CI over frames built byte by
//! byte in this source. The corpus tier is `#[ignore]`d, finds the corpus through
//! `ASTROFRAME_CORPUS`, and skips cleanly when that is unset — the corpus is 84 GB of real
//! observatory output that exists on one machine and is never committed.

#![forbid(unsafe_code)]

mod common;

use std::fmt::Debug;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::{Path, PathBuf};

use astroframe::{Limits, Reader, SampleFormat, Samples, Source};
use common::{Hdu, file};
use fitsrs::{Fits, HDU, Pixels};

// ------------------------------------------------------------------ the compared value

/// One image's native samples, in the file's own storage type.
///
/// The float variants hold `to_bits()` patterns rather than `f32`/`f64`, so the derived
/// comparison is a bit comparison: `==` on floats would call two NaNs unequal and a `+0.0`
/// equal to a `-0.0`, and a differential check that cannot see a sign bit is not one.
#[derive(Debug, PartialEq, Eq)]
enum Native {
    U8(Vec<u8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<u32>),
    F64(Vec<u64>),
}

impl Native {
    fn len(&self) -> usize {
        match self {
            Native::U8(v) => v.len(),
            Native::I16(v) => v.len(),
            Native::I32(v) => v.len(),
            Native::I64(v) => v.len(),
            Native::F32(v) => v.len(),
            Native::F64(v) => v.len(),
        }
    }
}

impl From<Samples> for Native {
    fn from(s: Samples) -> Native {
        match s {
            Samples::U8(v) => Native::U8(v),
            Samples::I16(v) => Native::I16(v),
            Samples::I32(v) => Native::I32(v),
            Samples::I64(v) => Native::I64(v),
            Samples::F32(v) => Native::F32(v.into_iter().map(f32::to_bits).collect()),
            Samples::F64(v) => Native::F64(v.into_iter().map(f64::to_bits).collect()),
            // FITS stores signed integers and unsigned bytes only; the unsigned convention
            // lives in layer 2, above native samples, so no FITS position produces these.
            other => panic!("no FITS position stores {:?} natively", other.format()),
        }
    }
}

// ------------------------------------------------------------------ the two readers

/// Every decodable image position this crate reports, in file order, plus how many positions
/// it declined.
///
/// A declined position is reported rather than skipped silently: it desynchronizes the two
/// sides' image ordinals, so a caller comparing lists has to know it happened.
fn astroframe_natives<S: Source>(
    reader: &mut Reader<S>,
) -> astroframe::Result<(Vec<Native>, usize)> {
    let mut out = Vec::new();
    let mut declined = 0usize;
    while reader.next_image()? {
        let header = reader.current_header().expect("the advanced position");
        if header.decline_reason().is_some() {
            declined += 1;
            continue;
        }
        let format = header
            .sample_format()
            .expect("a decodable position has one");
        let len = header.width().expect("geometry") as usize
            * header.height().expect("geometry") as usize
            * header.channels().expect("geometry") as usize;
        let mut samples = Samples::zeroed(format, len);
        reader.read_samples_into(&mut samples)?;
        out.push(Native::from(samples));
    }
    Ok((out, declined))
}

/// Every image HDU `fitsrs` reports, in file order.
///
/// Zero-pixel image HDUs are dropped — a `NAXIS = 0` primary is a header carrier, not an
/// image, and this crate does not advance to one — and table extensions are not images at all.
///
/// `fitsrs`'s sample iterator latches: once it yields an `Err` it returns `None` forever
/// after, so a truncated data unit surfaces as a short vector rather than an error. The length
/// assertion at the comparison site is what catches that.
fn fitsrs_natives<R>(source: R) -> Result<Vec<Native>, fitsrs::error::Error>
where
    R: Read + Seek + Debug + 'static,
{
    let mut hdu_list = Fits::from_reader(source);
    let mut out = Vec::new();
    while let Some(hdu) = hdu_list.next() {
        let hdu = hdu?;
        let image_hdu = match hdu {
            HDU::Primary(image_hdu) | HDU::XImage(image_hdu) => image_hdu,
            HDU::XBinaryTable(_) | HDU::XASCIITable(_) => continue,
        };
        let pixels = hdu_list.get_data(&image_hdu);
        let native = match pixels.pixels() {
            Pixels::U8(it) => Native::U8(it.collect()),
            Pixels::I16(it) => Native::I16(it.collect()),
            Pixels::I32(it) => Native::I32(it.collect()),
            Pixels::I64(it) => Native::I64(it.collect()),
            Pixels::F32(it) => Native::F32(it.map(f32::to_bits).collect()),
            Pixels::F64(it) => Native::F64(it.map(f64::to_bits).collect()),
        };
        if native.len() > 0 {
            out.push(native);
        }
    }
    Ok(out)
}

/// Compare the two lists position for position.
#[track_caller]
fn assert_same_natives(ours: &[Native], theirs: &[Native], what: &str) {
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{what}: the two readers disagree on how many images the file holds"
    );
    for (i, (a, b)) in ours.iter().zip(theirs).enumerate() {
        assert_eq!(
            a.len(),
            b.len(),
            "{what}: image {i}: sample counts differ — {} against {}",
            a.len(),
            b.len()
        );
        assert_eq!(a, b, "{what}: image {i}: native samples differ");
    }
}

// ------------------------------------------------------------------ fixtures

fn be_bytes<T: Copy, const N: usize>(values: &[T], to_be: impl Fn(T) -> [u8; N]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * N);
    for v in values {
        out.extend_from_slice(&to_be(*v));
    }
    out
}

/// `i16` samples chosen so a byte-swap is visible: every value has distinguishable halves, and
/// the sign is exercised in both directions.
const I16_SAMPLES: [i16; 8] = [-32768, -1, 0, 1, 258, 12345, 32767, -257];
const I32_SAMPLES: [i32; 8] = [-2147483648, -1, 0, 1, 66051, 305419896, 2147483647, -66051];
const I64_SAMPLES: [i64; 8] = [
    i64::MIN,
    -1,
    0,
    1,
    283686952306183,
    81985529216486895,
    i64::MAX,
    -283686952306183,
];
const U8_SAMPLES: [u8; 8] = [0, 1, 17, 127, 128, 200, 254, 255];

/// The float fixtures carry a NaN with a **non-canonical payload** and both zeros. Both
/// readers do one thing per sample — a big-endian read into the native type — so the payload
/// and the sign bit must survive on both sides; anything that canonicalizes a NaN or folds a
/// `-0.0` shows up here and nowhere else.
fn f32_samples() -> [f32; 8] {
    [
        -0.0,
        0.0,
        1.0,
        -1.5,
        f32::from_bits(0x7fc0_0001),
        f32::INFINITY,
        f32::NEG_INFINITY,
        3.4028235e38,
    ]
}

fn f64_samples() -> [f64; 8] {
    [
        -0.0,
        0.0,
        1.0,
        -1.5,
        f64::from_bits(0x7ff8_0000_0000_0001),
        f64::INFINITY,
        f64::NEG_INFINITY,
        1.7976931348623157e308,
    ]
}

/// One 4×2 frame per `BITPIX`, each carrying the unsigned convention where one exists so the
/// scaling keywords are present and provably ignored by both sides.
fn single_image_fixtures() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "BITPIX = 8",
            file(&[Hdu::primary()
                .image_2d(8, 4, 2)
                .unsigned_convention(8)
                .data_u8(&U8_SAMPLES)]),
        ),
        (
            "BITPIX = 16",
            file(&[Hdu::primary()
                .image_2d(16, 4, 2)
                .unsigned_convention(16)
                .data_i16(&I16_SAMPLES)]),
        ),
        (
            "BITPIX = 32",
            file(&[Hdu::primary()
                .image_2d(32, 4, 2)
                .unsigned_convention(32)
                .data(be_bytes(&I32_SAMPLES, i32::to_be_bytes))]),
        ),
        (
            "BITPIX = 64",
            file(&[Hdu::primary()
                .image_2d(64, 4, 2)
                .unsigned_convention(64)
                .data(be_bytes(&I64_SAMPLES, i64::to_be_bytes))]),
        ),
        (
            "BITPIX = -32",
            file(&[Hdu::primary()
                .image_2d(-32, 4, 2)
                .data(be_bytes(&f32_samples(), f32::to_be_bytes))]),
        ),
        (
            "BITPIX = -64",
            file(&[Hdu::primary()
                .image_2d(-64, 4, 2)
                .data(be_bytes(&f64_samples(), f64::to_be_bytes))]),
        ),
        (
            // NAXIS = 3 is read as channels, and the data unit is the same flat run of
            // samples either way — so the two readers must still agree sample for sample.
            "BITPIX = 16, NAXIS = 3",
            file(&[Hdu::primary()
                .image_3d(16, 2, 2, 3)
                .unsigned_convention(16)
                .data(be_bytes(
                    &(0..12i32)
                        .map(|i| (i * 3001 - 16384) as i16)
                        .collect::<Vec<i16>>(),
                    i16::to_be_bytes,
                ))]),
        ),
    ]
}

// ------------------------------------------------------------------ fixture tier

/// Criterion *Differential FITS check*, one `BITPIX` at a time.
#[test]
fn native_samples_match_fitsrs_at_every_bitpix() {
    for (what, bytes) in single_image_fixtures() {
        let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
        let (ours, declined) = astroframe_natives(&mut reader).expect("decode native samples");
        assert_eq!(declined, 0, "{what}: this crate declines nothing here");
        assert_eq!(ours.len(), 1, "{what}: one image");
        // Non-emptiness is asserted explicitly: two empty lists compare equal, so a
        // comparison that decoded nothing on either side would otherwise pass.
        assert!(ours[0].len() >= 8, "{what}: the fixture's samples decoded");

        let theirs = fitsrs_natives(Cursor::new(bytes)).expect("fitsrs parses the fixture");
        assert_same_natives(&ours, &theirs, what);
    }
}

/// The same check across a multi-extension file — a `NAXIS = 0` primary followed by three
/// image extensions of differing `BITPIX`, which is where the two readers' *positioning* is
/// graded rather than only their sample decode.
#[test]
fn native_samples_match_fitsrs_across_a_multi_extension_file() {
    let bytes = file(&[
        // A header-carrying primary: no data unit, and no image position for either reader.
        Hdu::primary().card("BITPIX", "8").card("NAXIS", "0"),
        Hdu::extension("IMAGE")
            .image_2d(16, 4, 2)
            .card("PCOUNT", "0")
            .card("GCOUNT", "1")
            .unsigned_convention(16)
            .data_i16(&I16_SAMPLES),
        Hdu::extension("IMAGE")
            .image_2d(-32, 4, 2)
            .card("PCOUNT", "0")
            .card("GCOUNT", "1")
            .data(be_bytes(&f32_samples(), f32::to_be_bytes)),
        Hdu::extension("IMAGE")
            .image_2d(8, 4, 2)
            .card("PCOUNT", "0")
            .card("GCOUNT", "1")
            .unsigned_convention(8)
            .data_u8(&U8_SAMPLES),
    ]);

    let mut reader = Reader::seekable(Cursor::new(bytes.clone())).expect("construct");
    let (ours, declined) = astroframe_natives(&mut reader).expect("decode native samples");
    assert_eq!(declined, 0, "this crate declines nothing here");
    assert_eq!(
        ours.len(),
        3,
        "the NAXIS = 0 primary is a header carrier, not a fourth image"
    );
    assert_eq!(ours[0].len(), 8);
    assert!(
        matches!(ours[1], Native::F32(_)),
        "the extension's own BITPIX governs"
    );

    let theirs = fitsrs_natives(Cursor::new(bytes)).expect("fitsrs parses the fixture");
    assert_same_natives(&ours, &theirs, "multi-extension");
}

/// A sequential source decodes the same native samples as a seekable one, checked against
/// `fitsrs` rather than only against this crate's other path.
#[test]
fn native_samples_match_fitsrs_on_a_sequential_source() {
    for (what, bytes) in single_image_fixtures() {
        let mut reader = Reader::sequential(Cursor::new(bytes.clone())).expect("construct");
        let (ours, _) = astroframe_natives(&mut reader).expect("decode native samples");
        let theirs = fitsrs_natives(Cursor::new(bytes)).expect("fitsrs parses the fixture");
        assert_same_natives(&ours, &theirs, &format!("{what}, sequential"));
    }
}

// ------------------------------------------------------------------ corpus tier

/// Criterion *Differential FITS check*, corpus tier.
///
/// `#[ignore]`d and developer-invoked: the corpus is 84 GB, lives outside the repository, and
/// no CI lane may depend on it. Over real frames this catches header-parsing and endianness
/// errors a synthetic fixture cannot, because a fixture is built by the same understanding it
/// tests.
///
/// Run with `ASTROFRAME_CORPUS=/path/to/corpus cargo test --all-features -- --ignored
/// --nocapture`.
#[test]
#[ignore = "corpus tier: developer-invoked, needs ASTROFRAME_CORPUS"]
fn corpus_native_samples_match_fitsrs() {
    let Some(root) = std::env::var_os("ASTROFRAME_CORPUS") else {
        // Unset is a clean skip, not a failure: the corpus exists on one machine.
        eprintln!("ASTROFRAME_CORPUS is unset — skipping the corpus tier");
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.is_dir(),
        "ASTROFRAME_CORPUS names {}, which is not a directory",
        root.display()
    );

    let mut files = Vec::new();
    collect_fits(&root, &mut files);
    files.sort();

    // The corpus is trusted local data, and the default caps are sized for unvetted input; a
    // real master exceeding one of them would otherwise read as a differential failure.
    let mut limits = Limits::default();
    limits.total_samples = u64::MAX;
    limits.decoded_output_bytes = u64::MAX;
    limits.materialized_bytes = u64::MAX;
    limits.skipped_block_bytes = u64::MAX;

    let (mut compared, mut images, mut skipped_declined, mut skipped_fitsrs, mut skipped_ours) =
        (0usize, 0usize, 0usize, 0usize, 0usize);

    for path in &files {
        let name = path.display().to_string();

        let mut reader = match Reader::open_with_limits(path, limits) {
            Ok(reader) => reader,
            Err(e) => {
                skipped_ours += 1;
                eprintln!("skip (astroframe construct): {name}: {e}");
                continue;
            }
        };
        let (ours, declined) = match astroframe_natives(&mut reader) {
            Ok(pair) => pair,
            Err(e) => {
                skipped_ours += 1;
                eprintln!("skip (astroframe decode): {name}: {e}");
                continue;
            }
        };
        if declined > 0 {
            // A declined position desynchronizes the ordinals the two lists are matched on,
            // so the file is skipped and counted rather than compared position by position.
            skipped_declined += 1;
            eprintln!("skip ({declined} declined position(s)): {name}");
            continue;
        }

        let source = match File::open(path) {
            Ok(f) => BufReader::new(f),
            Err(e) => {
                skipped_ours += 1;
                eprintln!("skip (open): {name}: {e}");
                continue;
            }
        };
        let theirs = match fitsrs_natives(source) {
            Ok(v) => v,
            Err(e) => {
                skipped_fitsrs += 1;
                eprintln!("skip (fitsrs): {name}: {e}");
                continue;
            }
        };

        assert_same_natives(&ours, &theirs, &name);
        compared += 1;
        images += ours.len();
    }

    eprintln!(
        "corpus differential: {compared} file(s) compared, {images} image(s); \
         skipped {skipped_declined} declined, {skipped_fitsrs} unparseable by fitsrs, \
         {skipped_ours} unreadable here; {} candidate(s) found under {}",
        files.len(),
        root.display()
    );
    assert!(
        compared > 0,
        "no FITS file under {} could be compared — the corpus path is probably wrong",
        root.display()
    );
}

/// Every FITS file under `dir`, recursively. Extensions are matched case-insensitively: the
/// corpus carries `.FIT` alongside `.fits`, and XISF files it must not pick up.
fn collect_fits(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_fits(&path, out);
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "fits" | "fit" | "fts") {
            out.push(path);
        }
    }
}

/// Guards the assumption the whole comparison rests on: `SampleFormat` is what this crate
/// reports natively for a FITS `BITPIX`, and every one of them maps onto a `fitsrs` `Pixels`
/// variant. A new `BITPIX` mapping that this suite could not compare would fail here rather
/// than quietly compare nothing.
#[test]
fn every_fits_sample_format_has_a_fitsrs_counterpart() {
    for (what, bytes) in single_image_fixtures() {
        let mut reader = Reader::seekable(Cursor::new(bytes)).expect("construct");
        assert!(reader.next_image().expect("advance"), "{what}: an image");
        let format = reader
            .current_header()
            .expect("header")
            .sample_format()
            .expect("a standard BITPIX");
        assert!(
            matches!(
                format,
                SampleFormat::U8
                    | SampleFormat::I16
                    | SampleFormat::I32
                    | SampleFormat::I64
                    | SampleFormat::F32
                    | SampleFormat::F64
            ),
            "{what}: {format:?} has no fitsrs Pixels counterpart"
        );
    }
}
