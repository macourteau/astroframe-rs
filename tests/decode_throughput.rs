//! Decode throughput, measured against the local corpus.
//!
//! **Not an acceptance criterion.** The design sets no throughput requirement and both
//! peak-memory criteria pass regardless of what this reports. It exists because the corpus
//! sweep surfaced a number worth having: a consumer reading "hundreds of frames per night" —
//! which the Problem section names — feels seconds per frame, and nothing else in the suite
//! would notice if a decode got ten times slower.
//!
//! `#[ignore]`d and reached through `ASTROFRAME_CORPUS`, like the rest of the corpus tier.
//! Single-threaded on purpose, so a profiler's trace is legible.
//!
//! ```text
//! ASTROFRAME_CORPUS=/path/to/corpus cargo test --release --test decode_throughput -- --ignored --nocapture
//! ```

use astroframe::{Reader, Samples};
use std::time::Instant;

/// One file per axis that plausibly costs something: uncompressed against compressed,
/// shuffled against not, and the widest sample type against the narrowest.
/// The pairs that matter are the ones differing **only** in whether a checksum is declared,
/// because that is what decides streaming against materializing: a checksum covers the whole
/// stored block, so a checksummed block cannot stream. Same bytes, same codec, two code paths.
const CASES: [&str; 10] = [
    "xisf_variants/uint16__none__none",
    "xisf_variants/float32__none__none",
    // zlib: streamed (no checksum) against materialized (checksummed)
    "xisf_variants/float32__zlib__none",
    "xisf_variants/float32__zlib__sha256",
    // zstd: the same pair
    "xisf_variants/float32__zstd__none",
    "xisf_variants/float32__zstd__sha256",
    // lz4 never streams, so both go through materialize -- the control
    "xisf_variants/float32__lz4__none",
    "xisf_variants/float32__lz4__sha256",
    "xisf_variants/float64__zlib__none",
    "xisf_variants/float64__zlib__sha256",
];

fn corpus() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(std::env::var("ASTROFRAME_CORPUS").ok()?);
    path.is_dir().then_some(path)
}

/// Decode every image of one file natively, returning the bytes produced and the time taken.
fn decode_one(path: &std::path::Path) -> Option<(u64, std::time::Duration)> {
    let mut reader = Reader::open(path).ok()?;
    let start = Instant::now();
    let mut bytes = 0u64;
    while reader.next_image().ok()? {
        let header = reader.header()?;
        if header.decline_reason().is_some() {
            continue;
        }
        let format = header.sample_format()?;
        let len =
            header.width()? as usize * header.height()? as usize * header.channels()? as usize;
        let mut samples = Samples::zeroed(format, len);
        reader.read_samples_into(&mut samples).ok()?;
        bytes += len as u64 * u64::from(format.bytes());
    }
    Some((bytes, start.elapsed()))
}

#[test]
#[ignore = "needs ASTROFRAME_CORPUS; measurement, not a criterion"]
fn decode_throughput_by_shape() {
    let Some(root) = corpus() else {
        eprintln!("skipping: ASTROFRAME_CORPUS is unset");
        return;
    };

    // A fixed number of repeats per case, so a slow case is not simply a bigger file.
    let repeats: usize = std::env::var("ASTROFRAME_BENCH_REPEATS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    println!(
        "{:<34} {:>9} {:>10} {:>12}",
        "case", "MB out", "seconds", "MB/s"
    );
    let mut total_bytes = 0u64;
    let mut total_time = std::time::Duration::ZERO;

    for case in CASES {
        let dir = root.join(case);
        // The same source frame in every directory, or the comparison measures file size.
        let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "xisf"))
            .collect();
        candidates.sort();
        let Some(path) = candidates.into_iter().next() else {
            println!("{case:<34} (absent)");
            continue;
        };

        let mut bytes = 0u64;
        let mut elapsed = std::time::Duration::ZERO;
        for _ in 0..repeats {
            let Some((b, t)) = decode_one(&path) else {
                println!("{case:<34} (did not decode)");
                break;
            };
            bytes += b;
            elapsed += t;
        }
        if bytes == 0 {
            continue;
        }
        let mb = bytes as f64 / 1_048_576.0;
        println!(
            "{:<34} {:>9.1} {:>10.2} {:>12.1}",
            case.rsplit('/').next().unwrap_or(case),
            mb,
            elapsed.as_secs_f64(),
            mb / elapsed.as_secs_f64()
        );
        total_bytes += bytes;
        total_time += elapsed;
    }

    let mb = total_bytes as f64 / 1_048_576.0;
    println!(
        "\n{:<34} {:>9.1} {:>10.2} {:>12.1}",
        "TOTAL",
        mb,
        total_time.as_secs_f64(),
        mb / total_time.as_secs_f64()
    );
}
