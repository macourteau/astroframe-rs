//! XISF data blocks: where they live, how they are compressed, and how they are verified.
//!
//! The pipeline order is fixed and nothing may be reordered to save a pass:
//!
//! > read the stored block → **verify** → decompress → unshuffle → de-interleave → normalize
//!
//! §10.6.1 is explicit that decompressing a block that failed verification is exploitable.

use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::xisf::scalars::{parse_u64, split_fields, trim};

/// How an embedded or inline block's bytes are spelled as text (§10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// `base64`.
    Base64,
    /// `hex` — Base16, lowercase digits only.
    Hex,
}

/// Where a block's bytes live (§10.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Location {
    /// `inline:<encoding>` — the bytes are this element's character data.
    Inline(Encoding),
    /// `embedded` — the bytes are a child `<Data>` element's character data.
    Embedded,
    /// `attachment:<position>:<size>`, and the `attached:` spelling the specification's own
    /// examples use.
    Attachment {
        /// Absolute byte offset in the monolithic file.
        position: u64,
        /// Stored length in bytes — compressed length when the block is compressed.
        size: u64,
    },
    /// `url(…)` or `path(…)`. §10.2 forbids external blocks in a monolithic unit outright, so
    /// this is `Malformed` rather than `Unsupported` wherever pixels depend on it.
    External(String),
}

/// Parse a `location` attribute.
///
/// **Both `attachment:` and `attached:` are accepted.** `attachment:` is normative (§10.3),
/// but four of the specification's own examples write `attached:`, and a writer that followed
/// the examples produces files that are otherwise valid. The spellings cannot be confused
/// with each other or with any other location form.
pub(crate) fn parse_location(text: &str) -> Result<Location> {
    let text = trim(text);
    if text == "embedded" {
        return Ok(Location::Embedded);
    }
    if let Some(encoding) = text.strip_prefix("inline:") {
        return Ok(Location::Inline(parse_encoding(encoding)?));
    }
    if text.starts_with("url(") || text.starts_with("path(") {
        return Ok(Location::External(text.to_owned()));
    }
    let rest = text
        .strip_prefix("attachment:")
        .or_else(|| text.strip_prefix("attached:"));
    if let Some(rest) = rest {
        let fields = split_fields(rest);
        if fields.len() != 2 {
            return Err(Error::malformed(format!(
                "location {text:?}: an attachment is spelled attachment:<position>:<size>"
            )));
        }
        let position = parse_u64(fields[0]).ok_or_else(|| {
            Error::malformed(format!(
                "location {text:?}: block position {:?} is not a non-negative integer",
                fields[0]
            ))
        })?;
        let size = parse_u64(fields[1]).ok_or_else(|| {
            Error::malformed(format!(
                "location {text:?}: block size {:?} is not a non-negative integer",
                fields[1]
            ))
        })?;
        return Ok(Location::Attachment { position, size });
    }
    Err(Error::malformed(format!(
        "location {text:?} is not one of the forms §10.3 defines"
    )))
}

/// Parse an `encoding` attribute value.
///
/// The specification is not silent on digit case, so there is nothing to guess at: an
/// uppercase Base16 spelling is rejected rather than accepted leniently.
pub(crate) fn parse_encoding(text: &str) -> Result<Encoding> {
    match trim(text) {
        "base64" => Ok(Encoding::Base64),
        "hex" => Ok(Encoding::Hex),
        other => Err(Error::malformed(format!(
            "encoding {other:?}: §10.3 defines base64 and hex"
        ))),
    }
}

/// A compression codec (§10.6.3–§10.6.8, plus `zstd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    /// zlib-wrapped, not raw deflate. A framed stream, so it decompresses incrementally.
    Zlib,
    /// A bare LZ4 block with no frame header, so it decompresses only as a whole.
    Lz4,
    /// LZ4 high-compression — the same bare block shape.
    Lz4Hc,
    /// Framed, and the only one of the three with a real magic number.
    ///
    /// **Corpus-derived rather than specified**: `zstd` appears nowhere in XISF 1.0, and its
    /// attribute syntax (`zstd:<size>`, `zstd+sh:<size>:<item-size>`) was established by
    /// reading attachment bytes. PixInsight writes these blocks, so declining them would make
    /// the crate fail on real output to preserve a boundary that was not at stake.
    Zstd,
}

/// A parsed `compression` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Compression {
    /// Which codec.
    pub(crate) codec: Codec,
    /// Whether the `+sh` byte-shuffling variant was applied before compression.
    pub(crate) shuffled: bool,
    /// The declared uncompressed length — a **cross-check** against the geometry-implied
    /// size, never an allocation size.
    pub(crate) uncompressed_size: u64,
    /// The mandatory third field of a `+sh` codec.
    pub(crate) item_size: Option<u64>,
}

/// Parse a `compression` attribute.
///
/// `item-size` comes from the attribute's mandatory third field and is **never derived from
/// `sampleFormat`**: §10.6 defines it only as "the length in bytes of a data item" and never
/// ties it to the sample width, so a `+sh` codec missing it is `Malformed` rather than a case
/// for inference.
pub(crate) fn parse_compression(text: &str) -> Result<Compression> {
    let text = trim(text);
    let fields = split_fields(text);
    let name = fields.first().copied().unwrap_or("");
    let (codec_name, shuffled) = match name.strip_suffix("+sh") {
        Some(base) => (base, true),
        None => (name, false),
    };
    let codec = match codec_name {
        "zlib" => Codec::Zlib,
        "lz4" => Codec::Lz4,
        "lz4hc" => Codec::Lz4Hc,
        "zstd" => Codec::Zstd,
        other => {
            return Err(Error::unsupported(format!(
                "compression codec {other:?}: this version reads zlib, lz4, lz4hc and zstd, \
                 each with its +sh variant"
            )));
        }
    };

    let want = if shuffled { 3 } else { 2 };
    if fields.len() != want {
        return Err(Error::malformed(format!(
            "compression {text:?}: {codec_name} takes {} fields",
            want - 1
        )));
    }
    let uncompressed_size = parse_u64(fields[1]).ok_or_else(|| {
        Error::malformed(format!(
            "compression {text:?}: uncompressed size {:?} is not a non-negative integer",
            fields[1]
        ))
    })?;

    let item_size = if shuffled {
        let n = parse_u64(fields[2]).ok_or_else(|| {
            Error::malformed(format!(
                "compression {text:?}: item size {:?} is not a non-negative integer",
                fields[2]
            ))
        })?;
        if n == 0 {
            return Err(Error::malformed(format!(
                "compression {text:?}: an item size of zero describes no transform"
            )));
        }
        Some(n)
    } else {
        None
    };

    Ok(Compression {
        codec,
        shuffled,
        uncompressed_size,
        item_size,
    })
}

/// One entry of a `subblocks` list: `(compressed, uncompressed)` lengths in bytes.
pub(crate) type Subblock = (u64, u64);

/// Parse a `subblocks` attribute — `c_1,u_1:c_2,u_2:…:c_N,u_N`.
///
/// §10.6 requires no validation of this list and explicitly sets **no** upper limit on the
/// number of subblocks, so three checks are added here. Without them the attribute is a cheap
/// amplification vector the element-count cap does not cover, the whole list being one
/// attribute string rather than elements. The count is capped here; the two sum checks are in
/// [`check_subblock_sums`], and all three run before any allocation.
pub(crate) fn parse_subblocks(text: &str, limits: &Limits) -> Result<Vec<Subblock>> {
    let text = trim(text);
    // Sized from the separator count rather than grown by doubling. This is not an allocation
    // from a *declared* size — the figure comes from bytes the source actually produced, and
    // it is clamped to the cap that bounds the list anyway — so invariant I4 is untouched,
    // while the doubling it removes is real: parsing 4096 pairs cost about 64 KB of
    // reallocation copies, charged against a header far smaller than the buffer.
    let expected =
        (text.bytes().filter(|b| *b == b':').count() + 1).min(limits.subblock_count as usize);
    let mut out = Vec::with_capacity(expected);
    for pair in text.split(':') {
        if out.len() as u64 >= u64::from(limits.subblock_count) {
            return Err(Error::limit(format!(
                "subblock count: the list declares more than the {} subblocks the cap allows",
                limits.subblock_count
            )));
        }
        let (c, u) = trim(pair).split_once(',').ok_or_else(|| {
            Error::malformed(format!(
                "subblocks {text:?}: {pair:?} is not a compressed,uncompressed pair"
            ))
        })?;
        let c = parse_u64(trim(c)).ok_or_else(|| {
            Error::malformed(format!("subblocks {text:?}: {c:?} is not a length"))
        })?;
        let u = parse_u64(trim(u)).ok_or_else(|| {
            Error::malformed(format!("subblocks {text:?}: {u:?} is not a length"))
        })?;
        out.push((c, u));
    }
    if out.is_empty() {
        return Err(Error::malformed("subblocks: the list is empty"));
    }
    Ok(out)
}

/// The two sum checks, run before any allocation.
pub(crate) fn check_subblock_sums(
    subblocks: &[Subblock],
    stored_size: u64,
    geometry_implied: u64,
) -> Result<()> {
    let mut compressed: u64 = 0;
    let mut uncompressed: u64 = 0;
    for (c, u) in subblocks {
        compressed = compressed
            .checked_add(*c)
            .ok_or_else(|| Error::malformed("subblocks: the compressed lengths overflow u64"))?;
        uncompressed = uncompressed
            .checked_add(*u)
            .ok_or_else(|| Error::malformed("subblocks: the uncompressed lengths overflow u64"))?;
    }
    if compressed != stored_size {
        return Err(Error::malformed(format!(
            "subblocks: the declared compressed lengths sum to {compressed}, but the stored \
             block is {stored_size} bytes"
        )));
    }
    if uncompressed != geometry_implied {
        return Err(Error::malformed(format!(
            "subblocks: the declared uncompressed lengths sum to {uncompressed}, but the \
             geometry implies {geometry_implied} bytes"
        )));
    }
    Ok(())
}

/// A cryptographic hashing algorithm (§10.5 Table 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Algorithm {
    /// `sha-1`, also `sha1`. The one §10.5 makes mandatory for a decoder claiming checksum
    /// support.
    Sha1,
    /// `sha-256`, also `sha256`.
    Sha256,
    /// `sha-512`, also `sha512`.
    Sha512,
    /// `sha3-256`.
    Sha3_256,
    /// `sha3-512`.
    Sha3_512,
}

/// A parsed `checksum` attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Checksum {
    /// Which algorithm.
    pub(crate) algorithm: Algorithm,
    /// The declared digest, decoded from its lowercase Base16 spelling.
    pub(crate) digest: Vec<u8>,
}

/// Parse a `checksum` attribute — `algorithm:digest`.
///
/// All five algorithms are supported rather than the mandatory one alone. A cheaper
/// sha1-only build would be conformant, but three hash crates is a small price beside a
/// feature matrix where a file's decodability depends on which digest its writer chose.
pub(crate) fn parse_checksum(text: &str) -> Result<Checksum> {
    let text = trim(text);
    let (name, digest) = text.split_once(':').ok_or_else(|| {
        Error::malformed(format!(
            "checksum {text:?}: §10.5 spells it algorithm:digest"
        ))
    })?;
    let algorithm = match trim(name) {
        "sha-1" | "sha1" => Algorithm::Sha1,
        "sha-256" | "sha256" => Algorithm::Sha256,
        "sha-512" | "sha512" => Algorithm::Sha512,
        "sha3-256" => Algorithm::Sha3_256,
        "sha3-512" => Algorithm::Sha3_512,
        other => {
            return Err(Error::unsupported(format!(
                "checksum algorithm {other:?}: §10.5 Table 9 defines sha-1, sha-256, sha-512, \
                 sha3-256 and sha3-512"
            )));
        }
    };
    // §10.5: digests are Base16 with the lowercase hexadecimal digits.
    let digest = decode_hex(trim(digest))
        .map_err(|_| Error::malformed(format!("checksum {text:?}: the digest is not Base16")))?;
    Ok(Checksum { algorithm, digest })
}

/// Decode lowercase Base16, ignoring XML white space.
///
/// §10.3 says white space "is irrelevant and *must* be ignored" for both text encodings, and
/// the specification's own embedded example is line-wrapped. Stripping happens **here**, at
/// the decode site, and never at the XML reader: §11.1.6 says the opposite for a `String`
/// property's character data, whose white space "a compliant decoder must preserve", and both
/// surfaces are read by the same parser.
pub(crate) fn decode_hex(text: &str) -> std::result::Result<Vec<u8>, ()> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|b| !crate::xisf::scalars::is_xml_space(*b))
        .collect();
    if !digits.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    // The length is already known even, so the remainder `as_chunks` returns is empty.
    for pair in digits.as_chunks::<2>().0 {
        let hi = hex_digit(pair[0])?;
        let lo = hex_digit(pair[1])?;
        out.push(hi << 4 | lo);
    }
    Ok(out)
}

fn hex_digit(b: u8) -> std::result::Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        // Lowercase only: §10.5 and §10.3 both spell Base16 with the lowercase digits, and
        // the specification is not silent on case, so there is nothing to guess at.
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(()),
    }
}

/// One byte of the **unshuffled** block, read without materializing an unshuffled copy.
///
/// The §10.6.2 transform stores the `item_size` subsets of equally significant bytes as
/// compact subsequences in ascending order, so byte `j` of item `i` sits at `j·N + i` for
/// `N = size / item_size` items. Reconstructing one sample therefore needs bytes spread
/// across the entire block and no prefix yields any complete sample — which is why shuffling
/// forces `WholeImage` granularity, ignoring any subblock split.
///
/// A **trailing partial item** is copied through unshuffled. The planes are defined over
/// "subsets of equally significant bytes", which exist only for complete items, so a partial
/// item belongs to no plane. That case is reachable on a conforming file precisely because
/// `item-size` is not tied to the sample width: a three-sample `UInt16` block with a legal
/// `item-size="4"` is six bytes with two left over.
///
/// Fusing the reverse transform into the per-sample read is a decision rather than an
/// optimization detail: it is what keeps `Block` granularity's peak at one block rather than
/// two.
#[inline]
pub(crate) fn unshuffled_byte(shuffled: &[u8], item_size: usize, offset: usize) -> u8 {
    if item_size <= 1 {
        // `item-size == 1` is a valid no-op: with one byte per item there is one plane, and
        // the transform is the identity.
        return shuffled[offset];
    }
    let items = shuffled.len() / item_size;
    let shuffled_span = items * item_size;
    let index = if offset < shuffled_span {
        let i = offset / item_size;
        let j = offset % item_size;
        j * items + i
    } else {
        offset
    };
    // Indexed rather than `get(..).unwrap_or(0)`. Every `offset` reaching here has been
    // bounded by `validate_row_range`, and `index` is a permutation of a bounded `offset`:
    // below `shuffled_span` it is `j * items + i` with `i < items` and `j < item_size`, so it
    // is below `items * item_size`. A fabricated zero pixel would be the silent repair this
    // crate refuses everywhere else in the pixel path.
    shuffled[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_accepts_both_attachment_spellings() {
        assert_eq!(
            parse_location("attachment:134217728:67108864").unwrap(),
            Location::Attachment {
                position: 134_217_728,
                size: 67_108_864
            }
        );
        // Four of the specification's own examples write `attached:`.
        assert_eq!(
            parse_location("attached:0:24").unwrap(),
            Location::Attachment {
                position: 0,
                size: 24
            }
        );
        assert_eq!(parse_location("embedded").unwrap(), Location::Embedded);
        assert_eq!(
            parse_location("inline:base64").unwrap(),
            Location::Inline(Encoding::Base64)
        );
        assert_eq!(
            parse_location("inline:hex").unwrap(),
            Location::Inline(Encoding::Hex)
        );
        assert!(matches!(
            parse_location("url(http://example.invalid/b)").unwrap(),
            Location::External(_)
        ));
        assert!(parse_location("attachment:0").is_err());
        assert!(parse_location("attachment:-1:8").is_err());
        assert!(parse_location("nowhere").is_err());
        // An uppercase Base16 spelling is rejected rather than accepted leniently.
        assert!(parse_location("inline:HEX").is_err());
    }

    #[test]
    fn compression_item_size_is_mandatory_for_sh_and_never_inferred() {
        let c = parse_compression("zlib:1024").unwrap();
        assert_eq!(c.codec, Codec::Zlib);
        assert!(!c.shuffled);
        assert_eq!(c.uncompressed_size, 1024);
        assert_eq!(c.item_size, None);

        let c = parse_compression("lz4+sh:1024:2").unwrap();
        assert_eq!(c.codec, Codec::Lz4);
        assert!(c.shuffled);
        assert_eq!(c.item_size, Some(2));

        // The corpus-derived zstd syntax.
        assert_eq!(parse_compression("zstd:64").unwrap().codec, Codec::Zstd);
        assert_eq!(
            parse_compression("zstd+sh:64:4").unwrap().item_size,
            Some(4)
        );

        // A `+sh` codec missing its third field is Malformed, not a case for inference.
        assert!(parse_compression("lz4+sh:1024").is_err());
        // Zero describes no transform.
        assert!(parse_compression("lz4+sh:1024:0").is_err());
        // An unknown codec is a declined feature rather than a broken file.
        assert!(matches!(
            parse_compression("brotli:16"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn subblock_sums_are_checked_against_the_stored_and_implied_sizes() {
        let s = parse_subblocks("10,20:30,40", &Limits::default()).unwrap();
        assert_eq!(s, vec![(10, 20), (30, 40)]);
        assert!(check_subblock_sums(&s, 40, 60).is_ok());
        assert!(check_subblock_sums(&s, 41, 60).is_err());
        assert!(check_subblock_sums(&s, 40, 61).is_err());

        let tight = Limits {
            subblock_count: 1,
            ..Limits::default()
        };
        assert!(matches!(
            parse_subblocks("10,20:30,40", &tight),
            Err(Error::LimitExceeded(_))
        ));
        assert!(parse_subblocks("10", &Limits::default()).is_err());
    }

    #[test]
    fn checksum_attribute_parses_both_spellings_of_each_algorithm() {
        let c = parse_checksum("sha1:97b25345e3bd74bcd6613d24e3ecb47617a31d20").unwrap();
        assert_eq!(c.algorithm, Algorithm::Sha1);
        assert_eq!(c.digest.len(), 20);
        assert_eq!(
            parse_checksum("sha-1:00").unwrap().algorithm,
            Algorithm::Sha1
        );
        assert_eq!(
            parse_checksum("sha-256:00").unwrap().algorithm,
            Algorithm::Sha256
        );
        assert_eq!(
            parse_checksum("sha3-512:00").unwrap().algorithm,
            Algorithm::Sha3_512
        );
        // Uppercase Base16 is not a conforming digest spelling.
        assert!(parse_checksum("sha1:9B").is_err());
        assert!(matches!(
            parse_checksum("md5:00"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn the_unshuffle_is_the_described_transform_and_copies_a_partial_item_through() {
        // Three 2-byte items, shuffled: all the low-significance bytes, then all the high.
        let original: [u8; 6] = [1, 2, 3, 4, 5, 6];
        let shuffled: [u8; 6] = [1, 3, 5, 2, 4, 6];
        for (offset, want) in original.iter().enumerate() {
            assert_eq!(
                unshuffled_byte(&shuffled, 2, offset),
                *want,
                "offset {offset}"
            );
        }

        // item-size == 1 is a valid no-op.
        for offset in 0..6 {
            assert_eq!(unshuffled_byte(&original, 1, offset), original[offset]);
        }

        // A trailing partial item is copied through unshuffled. Six bytes at item-size 4 is
        // one complete item and two bytes over -- reachable on a conforming file, since
        // item-size is not tied to the sample width.
        let shuffled: [u8; 6] = [1, 2, 3, 4, 5, 6];
        assert_eq!(unshuffled_byte(&shuffled, 4, 0), 1);
        assert_eq!(unshuffled_byte(&shuffled, 4, 3), 4);
        assert_eq!(
            unshuffled_byte(&shuffled, 4, 4),
            5,
            "partial item passes through"
        );
        assert_eq!(
            unshuffled_byte(&shuffled, 4, 5),
            6,
            "partial item passes through"
        );
    }

    #[test]
    fn base16_ignores_xml_white_space_and_rejects_uppercase() {
        assert_eq!(decode_hex("00ff\n 10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(decode_hex("0F").is_err());
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
