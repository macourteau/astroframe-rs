//! `Read` versus `Read + Seek`.
//!
//! XISF locates attached blocks by absolute file offset, so reaching one from a non-seekable
//! source means discarding bytes until the cursor arrives — which works only if the block
//! lies ahead. Rather than force `Seek` on every caller and lock out pipes and sockets, the
//! reader is generic over this abstraction, with two implementations. FITS never needs
//! `Seek` in either mode.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{Error, Result};
use crate::limits::Limits;

/// A byte source the reader walks forward through.
///
/// Implemented by [`Sequential`] and [`Seekable`]; not an extension point.
pub trait Source: sealed::Sealed {
    /// The current byte offset from the start of the source.
    fn position(&self) -> u64;

    /// The total length, when the source knows it. `None` for a sequential source, which is
    /// what makes the caps the floor rather than a backstop there.
    fn length(&self) -> Option<u64>;

    /// Whether the cursor can move backwards.
    fn is_seekable(&self) -> bool;

    /// Fill `buf` entirely.
    ///
    /// A short read is [`Error::Malformed`], not [`Error::Io`]: a short file is bad data, not
    /// a failing disk.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()>;

    /// Read up to `buf.len()` bytes, returning how many. `0` means end of source.
    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Move the cursor forward by `n` bytes.
    ///
    /// On a **sequential** source this is a read-and-discard, so it is bounded by
    /// [`Limits::skipped_block_bytes`] — over a pipe there is no end of file to terminate an
    /// enormous declared skip, and an unbounded read is the hang invariant I5 names. On a
    /// **seekable** source the cursor moves without transferring bytes and the cap is not
    /// consulted, so an ordinary MEF whose `BINTABLE` heap exceeds it is walked past rather
    /// than declined.
    fn skip(&mut self, n: u64, limits: &Limits) -> Result<()>;

    /// Move the cursor to an absolute position.
    ///
    /// Backwards on a sequential source is [`Error::Unsupported`] — the same file decodes
    /// through `Reader::open`.
    fn seek_to(&mut self, pos: u64, limits: &Limits) -> Result<()>;
}

mod sealed {
    pub trait Sealed {}
}

/// The discard buffer size for a sequential skip. Fixed, so a declared skip length never
/// becomes an allocation.
const SKIP_CHUNK: usize = 64 * 1024;

/// Forward-only. Skipping is read-and-discard; a block behind the cursor is
/// [`Error::Unsupported`], never a silent buffer.
#[derive(Debug)]
pub struct Sequential<R> {
    inner: R,
    pos: u64,
    scratch: Vec<u8>,
}

impl<R: Read> Sequential<R> {
    pub(crate) fn new(inner: R) -> Self {
        Sequential {
            inner,
            pos: 0,
            scratch: Vec::new(),
        }
    }
}

impl<R> sealed::Sealed for Sequential<R> {}

impl<R: Read> Source for Sequential<R> {
    fn position(&self) -> u64 {
        self.pos
    }

    fn length(&self) -> Option<u64> {
        None
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        read_exact_into(&mut self.inner, buf, &mut self.pos)
    }

    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize> {
        read_some_into(&mut self.inner, buf, &mut self.pos)
    }

    fn skip(&mut self, n: u64, limits: &Limits) -> Result<()> {
        if n > limits.skipped_block_bytes {
            return Err(Error::limit(format!(
                "skipped block bytes: {n} exceeds the {} byte cap, and this source must read \
                 in order to skip",
                limits.skipped_block_bytes
            )));
        }
        if self.scratch.is_empty() {
            self.scratch = vec![0u8; SKIP_CHUNK];
        }
        let mut left = n;
        while left > 0 {
            let want = left.min(SKIP_CHUNK as u64) as usize;
            let got = read_some_into(&mut self.inner, &mut self.scratch[..want], &mut self.pos)?;
            if got == 0 {
                return Err(Error::malformed(format!(
                    "truncated: {left} bytes short while skipping {n} bytes from offset {}",
                    self.pos
                )));
            }
            left -= got as u64;
        }
        Ok(())
    }

    fn seek_to(&mut self, pos: u64, limits: &Limits) -> Result<()> {
        match pos.checked_sub(self.pos) {
            Some(forward) => self.skip(forward, limits),
            None => Err(Error::unsupported(format!(
                "block at offset {pos} lies behind this sequential source's cursor at {}; \
                 the same file decodes through a seekable source",
                self.pos
            ))),
        }
    }
}

/// Skipping is a seek; block order does not matter, and the length is known.
#[derive(Debug)]
pub struct Seekable<R> {
    inner: R,
    pos: u64,
    len: u64,
}

impl<R: Read + Seek> Seekable<R> {
    pub(crate) fn new(mut inner: R) -> Result<Self> {
        let pos = inner.stream_position()?;
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(pos))?;
        Ok(Seekable { inner, pos, len })
    }
}

impl<R> sealed::Sealed for Seekable<R> {}

impl<R: Read + Seek> Source for Seekable<R> {
    fn position(&self) -> u64 {
        self.pos
    }

    fn length(&self) -> Option<u64> {
        Some(self.len)
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        read_exact_into(&mut self.inner, buf, &mut self.pos)
    }

    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize> {
        read_some_into(&mut self.inner, buf, &mut self.pos)
    }

    fn skip(&mut self, n: u64, limits: &Limits) -> Result<()> {
        let target = self.pos.checked_add(n).ok_or_else(|| {
            Error::malformed(format!(
                "skip of {n} bytes from offset {} overflows the source's address space",
                self.pos
            ))
        })?;
        self.seek_to(target, limits)
    }

    fn seek_to(&mut self, pos: u64, _limits: &Limits) -> Result<()> {
        // A seek to where the cursor already is is not free: `Seek for BufReader` discards its
        // read-ahead on every seek, inside the buffer or not, so a no-op seek throws away a
        // full buffer's worth of bytes already read. Every FITS decode does exactly one —
        // `begin_pixels` seeks to `data_start`, which is where the header parse left the
        // cursor. `self.pos` is maintained by every read and every seek, so this is exact
        // rather than an approximation of where the file is.
        if pos == self.pos {
            return Ok(());
        }
        // The cap bounds a read, not a seek: nothing is transferred here.
        self.inner.seek(SeekFrom::Start(pos))?;
        self.pos = pos;
        Ok(())
    }
}

fn read_exact_into<R: Read>(inner: &mut R, buf: &mut [u8], pos: &mut u64) -> Result<()> {
    let want = buf.len();
    let mut filled = 0usize;
    while filled < want {
        let got = read_some_into(inner, &mut buf[filled..], pos)?;
        if got == 0 {
            return Err(Error::malformed(format!(
                "truncated: wanted {want} bytes at offset {}, source ended after {filled}",
                *pos - filled as u64
            )));
        }
        filled += got;
    }
    Ok(())
}

fn read_some_into<R: Read>(inner: &mut R, buf: &mut [u8], pos: &mut u64) -> Result<usize> {
    // Unbounded retry on `Interrupted` is the conventional behaviour for a signal-interrupted
    // read, and it terminates here because the caller's own `Read` impl produces `Interrupted`,
    // not the file bytes I5 treats as hostile: a source that raised it forever would be a bug
    // in that impl, not a crafted input, so this loop does not need a progress argument against
    // adversarial data the way the decoders' loops do.
    loop {
        match inner.read(buf) {
            Ok(n) => {
                *pos += n as u64;
                return Ok(n);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
}
