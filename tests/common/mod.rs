//! Fixture builders.
//!
//! Fixtures are built **byte by byte here**, never checked in as opaque blobs. Every byte is
//! visible where the assertion is, which matters more than usual in this crate: a fixture
//! nobody can see cannot be reasoned about when an exhaustive bit-comparison fails.
//!
//! Everything built here is synthetic. No real frame header, and so no observatory
//! coordinates and no real `DATE-OBS`, ever reaches a committed file.

#![allow(dead_code)] // each integration test binary uses a different subset

pub mod xisf;

/// A FITS header unit is a whole number of these.
pub const BLOCK: usize = 2880;

/// Assemble one 80-byte FITS card.
///
/// Values are written in fixed format per FITS 4.0 §4.2 — right-justified in bytes 11–30 for
/// numbers and logicals, left-justified from byte 11 for quoted strings — so a third-party
/// decoder reads the same fixtures this crate does. That matters for the differential check.
pub fn card(name: &str, value: Option<&str>, comment: Option<&str>) -> [u8; 80] {
    let mut out = [b' '; 80];
    let name = name.as_bytes();
    assert!(name.len() <= 8, "keyword name {name:?} exceeds 8 bytes");
    out[..name.len()].copy_from_slice(name);

    let Some(value) = value else {
        // A commentary card: text runs from byte 8 to the end.
        if let Some(text) = comment {
            let text = text.as_bytes();
            let n = text.len().min(72);
            out[8..8 + n].copy_from_slice(&text[..n]);
        }
        return out;
    };

    out[8] = b'=';
    out[9] = b' ';
    let v = value.as_bytes();
    let end = if value.starts_with('\'') {
        // Left-justified from byte 11.
        let n = v.len().min(70);
        out[10..10 + n].copy_from_slice(&v[..n]);
        10 + n
    } else {
        // Right-justified, ending at byte 30.
        assert!(v.len() <= 20, "value {value:?} does not fit fixed format");
        let start = 30 - v.len();
        out[start..30].copy_from_slice(v);
        30
    };

    if let Some(text) = comment {
        let slash = end.max(31);
        if slash + 2 < 80 {
            out[slash] = b'/';
            out[slash + 1] = b' ';
            let text = text.as_bytes();
            let n = text.len().min(80 - slash - 2);
            out[slash + 2..slash + 2 + n].copy_from_slice(&text[..n]);
        }
    }
    out
}

/// A raw 80-byte card, for fixtures that need bytes a well-formed card would not carry.
pub fn raw_card(text: &[u8]) -> [u8; 80] {
    let mut out = [b' '; 80];
    let n = text.len().min(80);
    out[..n].copy_from_slice(&text[..n]);
    out
}

/// Builds one FITS header/data unit pair.
#[derive(Default)]
pub struct Hdu {
    cards: Vec<[u8; 80]>,
    data: Vec<u8>,
}

impl Hdu {
    /// A primary HDU, starting with the mandatory `SIMPLE = T`.
    pub fn primary() -> Self {
        let mut h = Hdu::default();
        h.cards.push(card("SIMPLE", Some("T"), None));
        h
    }

    /// A primary HDU whose `SIMPLE` card says the file does not conform.
    pub fn nonconforming_primary() -> Self {
        let mut h = Hdu::default();
        h.cards.push(card("SIMPLE", Some("F"), None));
        h
    }

    /// An `XTENSION` HDU.
    pub fn extension(kind: &str) -> Self {
        let mut h = Hdu::default();
        h.cards
            .push(card("XTENSION", Some(&format!("'{kind:<8}'")), None));
        h
    }

    /// Append a valued card.
    pub fn card(mut self, name: &str, value: &str) -> Self {
        self.cards.push(card(name, Some(value), None));
        self
    }

    /// Append a valued card carrying a comment.
    pub fn card_with_comment(mut self, name: &str, value: &str, comment: &str) -> Self {
        self.cards.push(card(name, Some(value), Some(comment)));
        self
    }

    /// Append a commentary card.
    pub fn commentary(mut self, name: &str, text: &str) -> Self {
        self.cards.push(card(name, None, Some(text)));
        self
    }

    /// Append a card verbatim.
    pub fn raw(mut self, bytes: &[u8]) -> Self {
        self.cards.push(raw_card(bytes));
        self
    }

    /// The geometry cards for a 2-D image, in the order the standard mandates.
    pub fn image_2d(self, bitpix: i32, width: u32, height: u32) -> Self {
        self.card("BITPIX", &bitpix.to_string())
            .card("NAXIS", "2")
            .card("NAXIS1", &width.to_string())
            .card("NAXIS2", &height.to_string())
    }

    /// The geometry cards for a 3-D image read as channels.
    pub fn image_3d(self, bitpix: i32, width: u32, height: u32, channels: u32) -> Self {
        self.card("BITPIX", &bitpix.to_string())
            .card("NAXIS", "3")
            .card("NAXIS1", &width.to_string())
            .card("NAXIS2", &height.to_string())
            .card("NAXIS3", &channels.to_string())
    }

    /// The FITS unsigned-integer convention for a given `BITPIX`.
    pub fn unsigned_convention(self, bitpix: i32) -> Self {
        let bzero = match bitpix {
            8 => "0",
            16 => "32768",
            32 => "2147483648",
            64 => "9223372036854775808",
            other => panic!("no unsigned convention for BITPIX = {other}"),
        };
        self.card("BSCALE", "1").card("BZERO", bzero)
    }

    /// Attach a data unit, which the builder pads to the block boundary.
    pub fn data(mut self, bytes: Vec<u8>) -> Self {
        self.data = bytes;
        self
    }

    /// Big-endian `i16` samples, the storage the unsigned convention uses at `BITPIX = 16`.
    pub fn data_i16(self, samples: &[i16]) -> Self {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            bytes.extend_from_slice(&s.to_be_bytes());
        }
        self.data(bytes)
    }

    /// Big-endian `u8` samples.
    pub fn data_u8(self, samples: &[u8]) -> Self {
        self.data(samples.to_vec())
    }

    /// Big-endian `f32` samples.
    pub fn data_f32(self, samples: &[f32]) -> Self {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            bytes.extend_from_slice(&s.to_be_bytes());
        }
        self.data(bytes)
    }

    /// Emit the header unit alone, without `END` — for fixtures that test its absence.
    pub fn header_without_end(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for c in &self.cards {
            out.extend_from_slice(c);
        }
        pad_to_block(&mut out);
        out
    }

    /// Emit the header unit and its padded data unit.
    pub fn emit(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for c in &self.cards {
            out.extend_from_slice(c);
        }
        out.extend_from_slice(&card("END", None, None));
        pad_to_block(&mut out);
        out.extend_from_slice(&self.data);
        pad_to_block(&mut out);
        out
    }
}

/// Concatenate HDUs into one file.
pub fn file(hdus: &[Hdu]) -> Vec<u8> {
    let mut out = Vec::new();
    for h in hdus {
        out.extend_from_slice(&h.emit());
    }
    out
}

fn pad_to_block(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(BLOCK) {
        out.push(b' ');
    }
}

/// Pad a data unit region with zeros rather than spaces, which is what the standard says for
/// a data unit as opposed to a header unit.
pub fn zero_pad(mut bytes: Vec<u8>) -> Vec<u8> {
    while !bytes.len().is_multiple_of(BLOCK) {
        bytes.push(0);
    }
    bytes
}

/// A `Read` source that records how many bytes were pulled from it, so a test can assert that
/// header-only decode touches only the header region.
pub struct CountingRead<R> {
    inner: R,
    pub read: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R: std::io::Read> CountingRead<R> {
    pub fn new(inner: R) -> (Self, std::rc::Rc<std::cell::Cell<u64>>) {
        let counter = std::rc::Rc::new(std::cell::Cell::new(0));
        (
            CountingRead {
                inner,
                read: counter.clone(),
            },
            counter,
        )
    }
}

impl<R: std::io::Read> std::io::Read for CountingRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.set(self.read.get() + n as u64);
        Ok(n)
    }
}

impl<R: std::io::Seek> std::io::Seek for CountingRead<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A source whose reads fail after `ok_bytes`, for the `Io` classification case.
pub struct FailingRead {
    pub data: Vec<u8>,
    pub pos: usize,
    pub ok_bytes: usize,
}

impl std::io::Read for FailingRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.ok_bytes {
            return Err(std::io::Error::other("simulated device failure"));
        }
        let n = buf
            .len()
            .min(self.ok_bytes - self.pos)
            .min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
