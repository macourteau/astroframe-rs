//! An XISF monolithic-unit builder.
//!
//! Same discipline as the FITS side: every byte is produced here, in test source, never
//! checked in as an opaque blob.
//!
//! **Two traps are inherited with the format and are handled here rather than left to each
//! fixture.** The attachment offset depends on the header length, which depends on the digit
//! count of the offset — so the position has to be iterated to a fixed point, and the loop
//! **asserts convergence** rather than silently giving up, which is the defect in the original
//! adversarial helper this suite ports from. And LZ4 fixtures need *compressible* sample
//! data: an LZ4 block compressor signals "incompressible" by producing no output, so random
//! pixels break a round-trip in a way that looks exactly like a decoder bug. Use
//! [`repeating_u16`] for those.

#![allow(dead_code)] // each integration test binary uses a different subset

/// The 8-byte signature, the little-endian header length, and the four reserved bytes.
pub const PREAMBLE: usize = 16;

/// One `<Image>` (or other block-bearing element) plus the bytes it attaches.
struct Attachment {
    /// The element text, with `{loc}` where the `location` attribute belongs.
    template: String,
    bytes: Vec<u8>,
}

/// Builds one monolithic XISF unit.
///
/// There is no `Default`: a unit with empty root attributes carries neither the namespace nor
/// the `version` §9.5 makes mandatory, so it is a fixture no test wants and every test could
/// reach. [`Unit::new`] is the only way in.
pub struct Unit {
    root_attrs: String,
    /// Root-level XML that carries no attachment, in document order relative to attachments.
    fragments: Vec<Fragment>,
}

enum Fragment {
    Xml(String),
    Attached(Attachment),
}

impl Unit {
    pub fn new() -> Self {
        Unit {
            root_attrs: r#" xmlns="http://www.pixinsight.com/xisf" version="1.0""#.to_owned(),
            fragments: Vec::new(),
        }
    }

    /// Replace the root element's attribute text wholesale — for the version, namespace and
    /// wrong-root fixtures.
    pub fn root_attrs(mut self, attrs: &str) -> Self {
        self.root_attrs = attrs.to_owned();
        self
    }

    /// Append root-level XML verbatim. Nothing is escaped: a fixture that wants a raw entity
    /// or a malformed tag writes it as it means it.
    pub fn xml(mut self, xml: &str) -> Self {
        self.fragments.push(Fragment::Xml(xml.to_owned()));
        self
    }

    /// Append an element whose block is attached after the header.
    ///
    /// `template` is the element text with `{loc}` standing in for the `location` attribute,
    /// which the builder computes once the header length settles. For example:
    ///
    /// ```text
    /// r#"<Image geometry="4:3:1" sampleFormat="UInt16" {loc}/>"#
    /// ```
    pub fn attached(mut self, template: &str, bytes: Vec<u8>) -> Self {
        assert!(
            template.contains("{loc}"),
            "an attached element's template must carry {{loc}}"
        );
        self.fragments.push(Fragment::Attached(Attachment {
            template: template.to_owned(),
            bytes: bytes.to_vec(),
        }));
        self
    }

    /// A single `UInt16` image over `data`, the common shape.
    pub fn image_u16(self, width: u32, height: u32, channels: u32, data: &[u16]) -> Self {
        let template = format!(
            r#"<Image geometry="{width}:{height}:{channels}" sampleFormat="UInt16" {{loc}}/>"#
        );
        self.attached(&template, le_u16(data))
    }

    /// Emit the unit.
    ///
    /// # Panics
    ///
    /// If the attachment offsets do not reach a fixed point. That assertion is the point: the
    /// offset's digit count feeds back into the header length that determines the offset, and
    /// a helper that gives up silently produces a fixture whose block is at the wrong place —
    /// which reads as a decoder bug.
    pub fn build(&self) -> Vec<u8> {
        let mut positions: Vec<u64> = self.attachment_sizes().iter().map(|_| 0).collect();
        let mut header = String::new();
        let mut converged = false;

        for _ in 0..16 {
            header = self.render(&positions);
            let base = PREAMBLE as u64 + header.len() as u64;
            let mut next = Vec::with_capacity(positions.len());
            let mut running = base;
            for size in self.attachment_sizes() {
                next.push(running);
                running += size;
            }
            if next == positions {
                converged = true;
                break;
            }
            positions = next;
        }
        assert!(
            converged,
            "attachment offsets did not converge: the offset's digit count feeds back into \
             the header length. Widen the loop or pad the header."
        );

        let mut out = Vec::with_capacity(PREAMBLE + header.len());
        out.extend_from_slice(b"XISF0100");
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(header.as_bytes());
        for fragment in &self.fragments {
            if let Fragment::Attached(a) = fragment {
                out.extend_from_slice(&a.bytes);
            }
        }
        out
    }

    /// Emit only the header region and the preamble — a size-capped prefix, for the
    /// header-only-decode-on-a-truncated-source criterion.
    pub fn build_header_only(&self) -> Vec<u8> {
        let whole = self.build();
        let declared = u32::from_le_bytes([whole[8], whole[9], whole[10], whole[11]]) as usize;
        whole[..PREAMBLE + declared].to_vec()
    }

    /// The declared header length of the unit this builder would emit.
    pub fn header_length(&self) -> u32 {
        let whole = self.build();
        u32::from_le_bytes([whole[8], whole[9], whole[10], whole[11]])
    }

    fn attachment_sizes(&self) -> Vec<u64> {
        self.fragments
            .iter()
            .filter_map(|f| match f {
                Fragment::Attached(a) => Some(a.bytes.len() as u64),
                Fragment::Xml(_) => None,
            })
            .collect()
    }

    fn render(&self, positions: &[u64]) -> String {
        let mut body = String::new();
        let mut i = 0;
        for fragment in &self.fragments {
            match fragment {
                Fragment::Xml(x) => body.push_str(x),
                Fragment::Attached(a) => {
                    let loc = format!(
                        r#"location="attachment:{}:{}""#,
                        positions.get(i).copied().unwrap_or(0),
                        a.bytes.len()
                    );
                    body.push_str(&a.template.replace("{loc}", &loc));
                    i += 1;
                }
            }
        }
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><xisf{}>{}</xisf>",
            self.root_attrs, body
        )
    }
}

/// The declared-header-length field, so a fixture that writes its own header region keeps the
/// preamble honest.
pub fn with_header(header: &str, trailing: &[u8]) -> Vec<u8> {
    raw_unit(b"XISF0100", header.len() as u32, header, trailing)
}

/// The 4 × 3 `UInt16` samples nearly every single-image fixture stores.
pub fn samples() -> Vec<u16> {
    repeating_u16(12)
}

/// The pinned normalization form for a `UInt16` image at the format default range, written
/// out longhand so an expectation never comes from the code under test.
pub fn expected_u16(levels: &[u16]) -> Vec<f32> {
    levels
        .iter()
        .map(|&l| (l as f64 - 0.0) as f32 * (1.0f32 / 65535.0f32))
        .collect()
}

/// Build a raw unit from a header string, bypassing the offset iteration.
///
/// For fixtures whose whole point is a header this builder would refuse to produce — a bad
/// signature, a declared length that overruns the file, unparseable XML.
pub fn raw_unit(signature: &[u8; 8], declared_len: u32, header: &str, trailing: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(signature);
    out.extend_from_slice(&declared_len.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(trailing);
    out
}

// ------------------------------------------------------------------ sample encoding

/// Little-endian `u16` samples — XISF's default byte order (§10.4).
pub fn le_u16(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Big-endian `u16` samples, for the `byteOrder="big"` fixture.
pub fn be_u16(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_be_bytes()).collect()
}

/// Little-endian `f32` samples.
pub fn le_f32(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Little-endian `u8` samples.
pub fn le_u8(samples: &[u8]) -> Vec<u8> {
    samples.to_vec()
}

/// Sample values that compress, for LZ4 fixtures.
///
/// An LZ4 block compressor signals "incompressible" by producing no output, so a fixture
/// built from random pixels breaks the round-trip in a way that looks exactly like a decoder
/// bug. A short repeating cycle avoids it. The cycle deliberately includes level 257, the
/// smallest level at which the multiply and divide normalization forms differ — a fixture
/// carrying none of those would pass against a divide-form implementation.
pub fn repeating_u16(len: usize) -> Vec<u16> {
    const CYCLE: [u16; 8] = [0, 257, 1, 261, 2, 265, 65535, 269];
    (0..len).map(|i| CYCLE[i % CYCLE.len()]).collect()
}

// ------------------------------------------------------------------ block transforms

/// The §10.6.2 byte-shuffling transform, written from the description: the `item_size`
/// subsets of equally significant bytes stored as compact subsequences in ascending order,
/// with a trailing partial item copied through unshuffled.
pub fn shuffle(input: &[u8], item_size: usize) -> Vec<u8> {
    assert!(item_size > 0);
    let items = input.len() / item_size;
    let mut out = Vec::with_capacity(input.len());
    for j in 0..item_size {
        for i in 0..items {
            out.push(input[i * item_size + j]);
        }
    }
    out.extend_from_slice(&input[items * item_size..]);
    out
}

/// zlib-wrapped deflate — **not** raw deflate, which is what an XISF `zlib` block holds.
pub fn zlib(input: &[u8]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(input).expect("zlib encode");
    e.finish().expect("zlib finish")
}

/// A bare LZ4 block — no frame header, which is what an XISF `lz4` block holds.
pub fn lz4(input: &[u8]) -> Vec<u8> {
    let out = lz4_flex::block::compress(input);
    assert!(
        !out.is_empty(),
        "the LZ4 compressor produced nothing; use repeating_u16 for compressible samples"
    );
    out
}

/// Base64, with no line wrapping.
pub fn base64(input: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(input)
}

/// Lowercase Base16, which is the only spelling §10.3 admits.
pub fn hex(input: &[u8]) -> String {
    use std::fmt::Write as _;
    input.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A `checksum` attribute value over the **stored** bytes — compressed ones when the block is
/// compressed (§10.6.1).
pub fn checksum_attr(algorithm: &str, stored: &[u8]) -> String {
    use sha1::Digest as _;
    let digest = match algorithm {
        "sha-1" | "sha1" => sha1::Sha1::digest(stored).to_vec(),
        "sha-256" | "sha256" => sha2::Sha256::digest(stored).to_vec(),
        "sha-512" | "sha512" => sha2::Sha512::digest(stored).to_vec(),
        "sha3-256" => sha3::Sha3_256::digest(stored).to_vec(),
        "sha3-512" => sha3::Sha3_512::digest(stored).to_vec(),
        other => panic!("unknown checksum algorithm {other}"),
    };
    format!(r#"checksum="{algorithm}:{}""#, hex(&digest))
}
