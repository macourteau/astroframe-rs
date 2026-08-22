//! *Peak decode memory meets the stated target* — the resident-memory half.
//!
//! The other half, the allocation bound in `tests/peak_memory.rs`, runs on every push. This
//! one is **scheduled**, because resident memory only means something on a quiet machine: it
//! measures what the operating system actually holds, which a busy runner and an allocator
//! that has not returned pages to the OS both perturb.
//!
//! Peak RSS is sampled by a thread polling during the decode rather than read from the
//! kernel's high-water mark afterwards, because that mark never decreases — building the
//! 52 MB fixture would leave it above anything the decode could show.
//!
//! Linux only. It **skips cleanly** elsewhere rather than failing, since `/proc` is where the
//! figure comes from and the scheduled lane runs on Linux runners.

mod common;

use astroframe::Reader;
use common::Hdu;
use common::xisf::{Unit, le_u16, lz4, repeating_u16};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const WIDTH: u32 = 6144;
const HEIGHT: u32 = 4096;

/// Current resident set size in bytes, or `None` where `/proc` does not provide it.
fn resident_bytes() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Run `body` while sampling RSS, and return its peak.
fn peak_during<F: FnOnce()>(body: F) -> usize {
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(resident_bytes().unwrap_or(0)));
    let sampler = {
        let stop = stop.clone();
        let peak = peak.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(rss) = resident_bytes() {
                    peak.fetch_max(rss, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    };
    body();
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    peak.load(Ordering::Relaxed)
}

/// Fault every page of the destination in **before** the baseline is read.
///
/// `vec![0.0f32; n]` is a zeroed mapping the kernel does not make resident until it is
/// written, so without this the destination's pages fault in *during* the decode and the
/// measurement charges the decode for the buffer it was told to fill. That is what made this
/// test read 104 MB over budget on its first real run.
fn touch(dst: &mut [f32]) {
    const PAGE: usize = 4096 / std::mem::size_of::<f32>();
    for i in (0..dst.len()).step_by(PAGE) {
        dst[i] = 0.0;
    }
}

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `#[ignore]`d so the per-push `test` job does not run it. Resident memory only means
/// something on a quiet machine — a runner building three other jobs perturbs it — which is
/// why the design puts this half on the scheduled lane. The scheduled job passes `--ignored`.
#[test]
#[ignore = "resident-memory measurement; scheduled lane only, needs a quiet machine"]
fn peak_resident_memory_meets_the_stated_target() {
    if resident_bytes().is_none() {
        eprintln!("skipping: resident-memory measurement needs /proc, so this runs on Linux only");
        return;
    }

    // ---- the FITS row: streaming should cost about half what materializing does ----
    let samples = WIDTH as usize * HEIGHT as usize;
    let path = std::env::temp_dir().join(format!("astroframe-rss-{}.fits", std::process::id()));
    {
        let stored: Vec<i16> = (0..samples)
            .map(|i| ((i % 65536) as i32 - 32768) as i16)
            .collect();
        let bytes = common::file(&[Hdu::primary()
            .image_2d(16, WIDTH, HEIGHT)
            .unsigned_convention(16)
            .data_i16(&stored)]);
        std::fs::write(&path, &bytes).expect("write the fixture");
    }
    let scratch = Scratch(path);

    let mut dst = vec![0.0f32; samples];
    let destination_bytes = dst.len() * 4;
    touch(&mut dst);
    let mut reader = Reader::open(&scratch.0).expect("open");
    assert!(reader.next_image().expect("advance"));

    let baseline = resident_bytes().expect("rss");
    let peak = peak_during(|| reader.read_image_into(&mut dst).expect("decode"));
    let above = peak.saturating_sub(baseline);
    let allowed = destination_bytes / 4; // 1.25x the destination, minus the destination
    // Printed because an RSS sample can legitimately come back at zero — the sampler is a
    // thread and the decode is short — and a bound satisfied by measuring nothing is a pass
    // that means nothing. The lane's log is where that shows.
    println!(
        "FITS 25 MP: peak {peak} baseline {baseline} above {above} allowed {allowed} \
         destination {destination_bytes}"
    );
    assert!(
        above <= allowed,
        "FITS decode peaked {above} bytes above a {baseline}-byte baseline with a \
         {destination_bytes}-byte destination; the 1.25x target allows {allowed}"
    );
    drop(reader);
    drop(dst);

    // ---- the compressed row: worse, because that is what this codec costs ----
    let width = 2048u32;
    let height = 2048u32;
    let samples = width as usize * height as usize;
    let raw = le_u16(&repeating_u16(samples));
    let template = format!(
        r#"<Image geometry="{width}:{height}:1" sampleFormat="UInt16" compression="lz4:{}" {{loc}}/>"#,
        raw.len()
    );
    let unit = Unit::new().attached(&template, lz4(&raw)).build();
    drop(raw);

    let mut dst = vec![0.0f32; samples];
    let destination_bytes = dst.len() * 4;
    touch(&mut dst);
    let mut reader = Reader::seekable(Cursor::new(&unit)).expect("open");
    assert!(reader.next_image().expect("advance"));

    let baseline = resident_bytes().expect("rss");
    let peak = peak_during(|| reader.read_image_into(&mut dst).expect("decode"));
    let above = peak.saturating_sub(baseline);
    let allowed = destination_bytes * 8 / 5; // 2.6x, minus the destination
    println!(
        "compressed XISF: peak {peak} baseline {baseline} above {above} allowed {allowed} \
         destination {destination_bytes}"
    );
    assert!(
        above <= allowed,
        "compressed XISF decode peaked {above} bytes above a {baseline}-byte baseline with a \
         {destination_bytes}-byte destination; the 2.6x target allows {allowed}"
    );
}
