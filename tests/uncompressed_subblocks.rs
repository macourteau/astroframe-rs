//! XISF 1.0 Revision 2's uncompressed subblocks (§10.6.1), graded through the public API on
//! every route a block can take.
//!
//! §10.6.1's rules, and where each is graded here:
//!
//! - A subblock with `c == u` is an **uncompressed subblock** whose stored bytes are its data,
//!   copied unmodified — among compressed ones, alone, empty, under byte shuffling (where it
//!   holds bytes of the *shuffled* block) and under a checksum (which covers it as stored):
//!   `uncompressed_subblocks_decode_alike_on_every_route_under_both_readings`.
//! - Every other subblock is decompressed, including one *longer* than its data: the same
//!   test's `longer than its data` pattern.
//! - Each compressed subblock decompresses to exactly `u` bytes, or the block is invalid:
//!   `a_compressed_subblock_of_the_wrong_length_is_malformed`.
//! - A block without `subblocks` is one subblock under the same rules:
//!   `an_unsplit_block_stored_at_its_uncompressed_size_is_copied`.
//! - A decoder *may* first try to decompress an equal-length subblock written by an earlier
//!   encoder, using the result only if it is exactly `u` bytes:
//!   `a_compressed_subblock_as_long_as_its_data_is_decompressed_only_on_request`, with the cap
//!   that attempt can trip in `a_cap_the_attempt_trips_is_reported_rather_than_read_as_pixels`
//!   and the granularity it costs in `try_decompress_reports_block_where_a_subblock_must_be_held`.
//!
//! A block's route is the header's choice for reasons of its own — attached and unchecksummed
//! streams, embedded and checksummed materialize — so every fixture runs on all three and the
//! outcome must be the block's. Fixtures are built byte by byte here; pixels are compared with
//! `f32::to_bits`.

#![cfg(feature = "xisf")]
#![forbid(unsafe_code)]

mod common;

use astroframe::{DecodeOptions, EqualLengthSubblocks, Reader};
use common::xisf::{
    Unit, block_routes, expected_u16, le_u16, lz4, outcome_under, repeating_u16, shuffle, zlib,
    zlib_unchecked, zstd_raw, zstd_rle,
};
use common::{Streams, assert_granularity, assert_same_bits};

// ------------------------------------------------------------------ fixtures

/// 32 × 8 `UInt16`: 512 bytes, enough to split into pieces that each compress.
const WIDTH: usize = 32;
const HEIGHT: usize = 8;
const IMAGE: &str = r#"geometry="32:8:1" sampleFormat="UInt16""#;

/// One codec as the fixtures need it.
struct Codec {
    name: &'static str,
    compress: fn(&[u8]) -> Vec<u8>,
    /// A compressed stream of no bytes, for a trailing pair owed nothing.
    empty: fn() -> Vec<u8>,
    /// Whether the codec is a framed stream, which streams by rows when nothing has to be held.
    framed: bool,
}

fn lz4_empty() -> Vec<u8> {
    // `lz4` refuses an empty block, which is the one this needs.
    lz4_flex::block::compress(&[])
}

fn codecs() -> [Codec; 4] {
    [
        Codec {
            name: "zlib",
            compress: zlib,
            empty: || zlib(&[]),
            framed: true,
        },
        Codec {
            name: "zstd",
            compress: zstd_raw,
            empty: || zstd_raw(&[]),
            framed: true,
        },
        Codec {
            name: "lz4",
            compress: lz4,
            empty: lz4_empty,
            framed: false,
        },
        // The same bare-block container written by a higher-effort compressor, so an ordinary
        // LZ4 block is a conforming `lz4hc` one.
        Codec {
            name: "lz4hc",
            compress: lz4,
            empty: lz4_empty,
            framed: false,
        },
    ]
}

const READINGS: [EqualLengthSubblocks; 2] = [
    EqualLengthSubblocks::Uncompressed,
    EqualLengthSubblocks::TryDecompress,
];

fn options(reading: EqualLengthSubblocks) -> DecodeOptions {
    DecodeOptions::default().with_equal_length_subblocks(reading)
}

/// One `(compressed, uncompressed)` pair of a fixture, by what it holds.
#[derive(Clone, Copy, Debug)]
enum Piece {
    /// The next `n` bytes of the block, stored as they are: `(n, n)`.
    Raw(usize),
    /// The next `n` bytes of the block, compressed: `(c, n)` with `c != n` either way.
    Packed(usize),
    /// `(0, 0)`.
    Empty,
    /// A compressed stream of no bytes: `(c, 0)` with `c > 0`.
    Trailing,
}

use Piece::{Empty, Packed, Raw, Trailing};

/// The split patterns every codec is read under. Each sums to the image's 512 bytes.
const PATTERNS: &[(&str, &[Piece])] = &[
    ("all raw", &[Raw(64); 8]),
    (
        "alternating",
        &[
            Raw(64),
            Packed(64),
            Raw(64),
            Packed(64),
            Packed(64),
            Raw(64),
            Packed(64),
            Raw(64),
        ],
    ),
    ("raw then packed", &[Raw(256), Packed(256)]),
    (
        "longer than its data",
        &[
            Packed(2),
            Raw(62),
            Packed(3),
            Raw(61),
            Packed(128),
            Raw(256),
        ],
    ),
    (
        "empty pairs",
        &[
            Empty,
            Raw(128),
            Empty,
            Packed(128),
            Empty,
            Empty,
            Raw(256),
            Trailing,
            Empty,
        ],
    ),
    ("one raw pair", &[Raw(512)]),
];

/// The block's bytes and its `subblocks` list, built from `pieces` over `block`.
fn split(codec: &Codec, pieces: &[Piece], block: &[u8]) -> (Vec<u8>, String) {
    let mut stored = Vec::new();
    let mut list = Vec::new();
    let mut at = 0usize;
    for piece in pieces {
        let (bytes, u) = match *piece {
            Raw(n) => (block[at..at + n].to_vec(), n),
            Packed(n) => {
                let c = (codec.compress)(&block[at..at + n]);
                assert_ne!(
                    c.len(),
                    n,
                    "{}: a packed piece must not be as long as its data",
                    codec.name
                );
                (c, n)
            }
            Empty => (Vec::new(), 0),
            Trailing => {
                let c = (codec.empty)();
                assert!(
                    !c.is_empty(),
                    "{}: a trailing pair must be a stream, not (0, 0)",
                    codec.name
                );
                (c, 0)
            }
        };
        at += u;
        list.push(format!("{},{u}", bytes.len()));
        stored.extend_from_slice(&bytes);
    }
    assert_eq!(at, block.len(), "the pieces cover the block");
    (stored, list.join(":"))
}

/// The `compression` attribute for `codec` over a 512-byte block.
fn compression(codec: &Codec, shuffled: bool) -> String {
    let size = WIDTH * HEIGHT * 2;
    if shuffled {
        format!(r#"compression="{}+sh:{size}:2""#, codec.name)
    } else {
        format!(r#"compression="{}:{size}""#, codec.name)
    }
}

/// The floor an attached, unchecksummed block of these pairs reports.
///
/// The shuffle spans the whole block whatever the split; a bare LZ4 block holds a subblock at
/// a time; and a framed codec streams by rows unless a pair must be held for an attempt, which
/// `TryDecompress` makes of every non-empty equal-length pair.
fn attached_floor(
    codec: &Codec,
    shuffled: bool,
    pieces: &[Piece],
    reading: EqualLengthSubblocks,
) -> Streams {
    let held = !codec.framed
        || (reading == EqualLengthSubblocks::TryDecompress
            && pieces.iter().any(|p| matches!(p, Raw(n) if *n > 0)));
    match () {
        _ if shuffled => Streams::WholeImage,
        _ if held => Streams::Block(pieces.len() as u32),
        _ => Streams::Rows,
    }
}

/// §10.6.3's inverse: the block bytes back in sample order, for fixtures built in the shuffled
/// domain.
fn unshuffle(block: &[u8], item_size: usize) -> Vec<u8> {
    let items = block.len() / item_size;
    let mut out = vec![0u8; block.len()];
    for j in 0..item_size {
        for i in 0..items {
            out[i * item_size + j] = block[j * items + i];
        }
    }
    out[items * item_size..].copy_from_slice(&block[items * item_size..]);
    out
}

/// The pixels a block's bytes decode to: unshuffled first when the block is shuffled.
fn pixels_of(block: &[u8], shuffled: bool) -> Vec<f32> {
    let bytes = if shuffled {
        unshuffle(block, 2)
    } else {
        block.to_vec()
    };
    let levels: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    expected_u16(&levels)
}

/// The image's own bytes as the block holds them.
fn image_block(shuffled: bool) -> Vec<u8> {
    let plain = le_u16(&repeating_u16(WIDTH * HEIGHT));
    let block = if shuffled { shuffle(&plain, 2) } else { plain };
    assert_eq!(
        unshuffle(&shuffle(&block, 2), 2),
        block,
        "the test's unshuffle inverts shuffle"
    );
    block
}

/// Every route of one block decodes to `want` under `reading`, reporting the route's floor.
fn decodes_on_every_route(
    block_attrs: &str,
    stored: &[u8],
    attached: Streams,
    reading: EqualLengthSubblocks,
    want: &[f32],
    what: &str,
) {
    for (route, unit, floor) in block_routes(IMAGE, block_attrs, stored, attached) {
        let what = format!("{what}, {route}, {reading:?}");
        let (granularity, got) = outcome_under(unit, &options(reading))
            .unwrap_or_else(|class| panic!("{what}: refused as {class}"));
        assert_same_bits(&got, want, &what);
        assert_granularity(granularity, floor, &what);
    }
}

// ------------------------------------------------------------------ the strict rules

/// §10.6.1's uncompressed subblocks, among compressed ones and alone, on every codec and its
/// `+sh` variant, under both readings, bit-compared across the three routes.
///
/// One expectation serves every route, so the routes agree by construction of the test rather
/// than by a second comparison. A raw pair under `TryDecompress` is attempted and falls back,
/// which is why the expectation is the same under it: none of these stored runs is a stream of
/// its codec that yields exactly its length.
#[test]
fn uncompressed_subblocks_decode_alike_on_every_route_under_both_readings() {
    for codec in codecs() {
        for shuffled in [false, true] {
            let block = image_block(shuffled);
            let want = pixels_of(&block, shuffled);
            for (pattern, pieces) in PATTERNS {
                let (stored, list) = split(&codec, pieces, &block);
                let attrs = format!(r#"{} subblocks="{list}""#, compression(&codec, shuffled));
                for reading in READINGS {
                    decodes_on_every_route(
                        &attrs,
                        &stored,
                        attached_floor(&codec, shuffled, pieces, reading),
                        reading,
                        &want,
                        &format!("{}, shuffled {shuffled}, {pattern}", codec.name),
                    );
                }
            }
        }
    }
}

/// A block without `subblocks` is one subblock (§10.6.1), so one stored at exactly its
/// uncompressed size is an uncompressed subblock and is copied.
///
/// Under the default a framed codec still streams it by rows, read straight from the source.
/// `TryDecompress` has to attempt it, which needs it resident, and a block with no split to
/// hold a piece of is held whole.
#[test]
fn an_unsplit_block_stored_at_its_uncompressed_size_is_copied() {
    for codec in codecs() {
        for shuffled in [false, true] {
            let block = image_block(shuffled);
            let want = pixels_of(&block, shuffled);
            for reading in READINGS {
                let attached = match () {
                    _ if shuffled || !codec.framed => Streams::WholeImage,
                    _ if reading == EqualLengthSubblocks::TryDecompress => Streams::WholeImage,
                    _ => Streams::Rows,
                };
                decodes_on_every_route(
                    &compression(&codec, shuffled),
                    &block,
                    attached,
                    reading,
                    &want,
                    &format!("{} unsplit, shuffled {shuffled}", codec.name),
                );
            }
        }
    }
}

/// §10.6.1: a decoder *shall* verify that each compressed subblock decompresses to exactly
/// its uncompressed length, and *shall* treat the block as invalid otherwise — short and long,
/// on every codec, route and reading. Each list still sums correctly, so the refusal is the
/// codec's and comes at the pixels.
#[test]
fn a_compressed_subblock_of_the_wrong_length_is_malformed() {
    for codec in codecs() {
        for shuffled in [false, true] {
            let block = image_block(shuffled);
            // The first pair declares 64 bytes and its stream holds 62, or 66.
            for (actual, what) in [(62usize, "short"), (66, "long")] {
                let first = (codec.compress)(&block[..actual]);
                let mut stored = first.clone();
                stored.extend_from_slice(&block[64..]);
                let rest = block.len() - 64;
                let attrs = format!(
                    r#"{} subblocks="{},64:{rest},{rest}""#,
                    compression(&codec, shuffled),
                    first.len()
                );
                for reading in READINGS {
                    for (route, unit, _) in
                        block_routes(IMAGE, &attrs, &stored, Streams::WholeImage)
                    {
                        let what = format!(
                            "{} {what}, shuffled {shuffled}, {route}, {reading:?}",
                            codec.name
                        );
                        let got = outcome_under(unit, &options(reading));
                        assert_eq!(got.err(), Some("Malformed"), "{what}");
                    }
                }
            }
        }
    }
}

/// A forward-only source reads uncompressed subblocks too: a raw pair after a compressed one
/// starts where the declared lengths say, which a codec that stopped short of its stored bytes
/// leaves ahead of the cursor, and only a forward move is ever needed.
#[test]
fn a_forward_only_source_reads_uncompressed_subblocks() {
    let block = image_block(false);
    let want = pixels_of(&block, false);
    for codec in codecs() {
        let (stored, list) = split(&codec, PATTERNS[1].1, &block);
        let unit = Unit::new()
            .attached(
                &format!(
                    r#"<Image {IMAGE} {} subblocks="{list}" {{loc}}/>"#,
                    compression(&codec, false)
                ),
                stored,
            )
            .build();
        let mut reader = Reader::sequential(std::io::Cursor::new(unit)).expect("constructs");
        assert!(reader.next_image().expect("advances"));
        let got = reader.read_image().expect("decodes").into_samples();
        assert_same_bits(
            &got,
            &want,
            &format!("{} over a forward-only source", codec.name),
        );
    }
}

// ------------------------------------------------------------------ the legacy reading

/// A compressed stream exactly as long as its data, found by search: `n` bytes made of a run of
/// pseudo-random bytes, a run of zeros, and pseudo-random bytes again, at the first mix the
/// codec compresses to exactly `n`. The zeros are what compresses, and moving the runs is what
/// lets the search land on the length: an LZ4 block, which must end in literals, skips over it
/// for every placement of the zeros at the end.
fn equal_length(compress: impl Fn(&[u8]) -> Vec<u8>, what: &str) -> (Vec<u8>, Vec<u8>) {
    let mut state = 0x2545_f491u32;
    let noise: Vec<u8> = (0..256)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    for n in 16..=64usize {
        for zeros in 1..n {
            for head in 0..=(n - zeros) {
                let mut data = noise[..head].to_vec();
                data.resize(head + zeros, 0);
                data.extend_from_slice(&noise[128..128 + (n - head - zeros)]);
                let packed = compress(&data);
                if packed.len() == n {
                    return (data, packed);
                }
            }
        }
    }
    panic!("no {what} stream as long as its data in the searched range");
}

/// The legacy pair each codec is graded with: `(data, stored)`, `stored` a genuine stream of
/// `data` with `stored.len() == data.len()`.
fn legacy_pair(codec: &Codec) -> (Vec<u8>, Vec<u8>) {
    match codec.name {
        // Ten bytes of frame and RLE block, decoding to ten bytes.
        "zstd" => (vec![0x01; 10], zstd_rle(0x01, 10)),
        "zlib" => equal_length(zlib_unchecked, "zlib"),
        _ => equal_length(|d: &[u8]| lz4_flex::block::compress(d), "LZ4"),
    }
}

/// Revision 2's §2.1 and §10.6.1 *may*: a compressed subblock of equal lengths, written by an
/// encoder that compressed every subblock, is decompressed under `TryDecompress` and read as
/// stored bytes otherwise.
///
/// Both halves are asserted, so neither reading can be the other. The block also carries a raw
/// pair, which `TryDecompress` attempts and falls back on, and a compressed one.
#[test]
fn a_compressed_subblock_as_long_as_its_data_is_decompressed_only_on_request() {
    for codec in codecs() {
        for shuffled in [false, true] {
            let (data, packed) = legacy_pair(&codec);
            assert_eq!(
                packed.len(),
                data.len(),
                "{}: the legacy fixture's lengths",
                codec.name
            );
            assert_ne!(packed, data, "{}: a stream, not its data", codec.name);
            let n = data.len();

            // The block in its stored domain: the legacy pair's data, then the image's own
            // bytes to fill it, split into a raw piece and a compressed one.
            let image = image_block(shuffled);
            let mut block = data.clone();
            block.extend_from_slice(&image[n..]);
            let rest = block.len() - n;
            let raw = rest / 2;
            let tail = &block[n + raw..];
            let packed_tail = (codec.compress)(tail);
            let mut stored = packed.clone();
            stored.extend_from_slice(&block[n..n + raw]);
            stored.extend_from_slice(&packed_tail);
            let attrs = format!(
                r#"{} subblocks="{n},{n}:{raw},{raw}:{},{}""#,
                compression(&codec, shuffled),
                packed_tail.len(),
                tail.len()
            );

            // What the default reads: the legacy pair's stored bytes, as they are.
            let mut as_stored = block.clone();
            as_stored[..n].copy_from_slice(&packed);

            let what = format!("{} legacy pair, shuffled {shuffled}", codec.name);
            let pieces = [Raw(n), Raw(raw), Packed(tail.len())];
            for (reading, want) in [
                (
                    EqualLengthSubblocks::TryDecompress,
                    pixels_of(&block, shuffled),
                ),
                (
                    EqualLengthSubblocks::Uncompressed,
                    pixels_of(&as_stored, shuffled),
                ),
            ] {
                decodes_on_every_route(
                    &attrs,
                    &stored,
                    attached_floor(&codec, shuffled, &pieces, reading),
                    reading,
                    &want,
                    &what,
                );
            }
        }
    }
}

/// The attempt falls back only when the codec rejects its input. A zstd frame header declaring
/// a window above `zstd_window_bytes` may be valid compressed data the cap refuses, so reading
/// it as pixels would fabricate them: under `TryDecompress` it is `LimitExceeded`, on every
/// route, and under the default it is sixteen bytes of data like any other.
#[test]
fn a_cap_the_attempt_trips_is_reported_rather_than_read_as_pixels() {
    // Magic, a frame header with no single-segment flag, and a window descriptor for 1 GiB —
    // above the 8 MiB default — then filler to the pair's length.
    let mut header = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 20 << 3];
    header.resize(16, 0x5a);
    let image = image_block(false);
    let mut stored = header.clone();
    stored.extend_from_slice(&image[16..]);
    let rest = image.len() - 16;
    let attrs = format!(r#"compression="zstd:512" subblocks="16,16:{rest},{rest}""#);

    let mut as_stored = image.clone();
    as_stored[..16].copy_from_slice(&header);
    decodes_on_every_route(
        &attrs,
        &stored,
        Streams::Rows,
        EqualLengthSubblocks::Uncompressed,
        &pixels_of(&as_stored, false),
        "a window-declaring pair, read as stored bytes",
    );
    for (route, unit, _) in block_routes(IMAGE, &attrs, &stored, Streams::Block(2)) {
        let got = outcome_under(unit, &options(EqualLengthSubblocks::TryDecompress));
        assert_eq!(got.err(), Some("LimitExceeded"), "{route}");
    }
}

/// `granularity()` comes from the header and the options, never from the source, and
/// `TryDecompress` moves it only where a pair has to be held for its attempt: a framed codec
/// with a non-empty equal-length pair reports `Block`, or `WholeImage` with no split to hold a
/// piece of. An empty pair has nothing to attempt and moves nothing.
#[test]
fn try_decompress_reports_block_where_a_subblock_must_be_held() {
    let block = image_block(false);
    let (front, back) = block.split_at(256);
    let (packed_front, packed_back, packed_all) = (zlib(front), zlib(back), zlib(&block));
    let (cf, cb, ca) = (packed_front.len(), packed_back.len(), packed_all.len());
    let cases = [
        (
            "a raw pair",
            format!(r#"compression="zlib:512" subblocks="256,256:{cb},256""#),
            [front, &packed_back].concat(),
            Streams::Rows,
            Streams::Block(2),
        ),
        (
            "no equal-length pair",
            format!(r#"compression="zlib:512" subblocks="{cf},256:{cb},256""#),
            [packed_front.as_slice(), &packed_back].concat(),
            Streams::Rows,
            Streams::Rows,
        ),
        (
            "an empty pair",
            format!(r#"compression="zlib:512" subblocks="0,0:{ca},512""#),
            packed_all.clone(),
            Streams::Rows,
            Streams::Rows,
        ),
        (
            "unsplit, stored at its size",
            r#"compression="zlib:512""#.to_owned(),
            block.clone(),
            Streams::Rows,
            Streams::WholeImage,
        ),
    ];
    for (what, attrs, stored, default, legacy) in cases {
        let unit = Unit::new()
            .attached(&format!("<Image {IMAGE} {attrs} {{loc}}/>"), stored)
            .build();
        for (reading, want) in [
            (EqualLengthSubblocks::Uncompressed, default),
            (EqualLengthSubblocks::TryDecompress, legacy),
        ] {
            let what = format!("{what}, {reading:?}");
            let mut reader = options(reading)
                .seekable(std::io::Cursor::new(unit.clone()))
                .expect("constructs");
            assert!(reader.next_image().expect("advances"));
            let got = reader.current_header().expect("a header").granularity();
            assert_granularity(got, want, &what);
            let samples = reader.read_image().expect("decodes").into_samples();
            assert_same_bits(&samples, &pixels_of(&block, false), &what);

            // A forward-only source reports the same: the source type is not an input to it.
            let mut reader = options(reading)
                .sequential(std::io::Cursor::new(unit.clone()))
                .expect("constructs");
            assert!(reader.next_image().expect("advances"));
            let got = reader.current_header().expect("a header").granularity();
            assert_granularity(got, want, &format!("{what}, forward-only"));
        }
    }
}
