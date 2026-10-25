//! Bounded buffer in front of a BLAKE3 hasher, shared by the payload hashers.

use std::io::{self, Read};

const FLUSH_AT: usize = 64 * 1024;

/// Buffers hashed bytes and flushes at `FLUSH_AT` so memory stays bounded.
pub(super) struct Sink<'a> {
    hasher: &'a mut blake3::Hasher,
    buf: Vec<u8>,
}

impl<'a> Sink<'a> {
    pub(super) fn new(hasher: &'a mut blake3::Hasher) -> Self {
        Self {
            hasher,
            buf: Vec::with_capacity(FLUSH_AT),
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= FLUSH_AT {
            self.flush();
        }
    }

    pub(super) fn flush(&mut self) {
        self.hasher.update(&self.buf);
        self.buf.clear();
    }

    /// Streams exactly `len` bytes from `r` into the hash, never allocating
    /// more than a small fixed buffer whatever `len` claims.
    /// Fails with `UnexpectedEof` if `r` ends early.
    pub(super) fn copy_from(&mut self, r: &mut impl Read, len: u64) -> io::Result<()> {
        let mut chunk = [0u8; 8192];
        let mut left = len;
        while left > 0 {
            let n = usize::try_from(left).map_or(chunk.len(), |l| l.min(chunk.len()));
            r.read_exact(&mut chunk[..n])?;
            self.push(&chunk[..n]);
            left -= n as u64;
        }
        Ok(())
    }
}

/// Reads and discards exactly `len` bytes; `UnexpectedEof` if `r` ends early.
pub(super) fn skip(r: &mut impl Read, len: u64) -> io::Result<()> {
    let copied = io::copy(&mut r.take(len), &mut io::sink())?;
    if copied == len {
        Ok(())
    } else {
        Err(io::ErrorKind::UnexpectedEof.into())
    }
}

#[cfg(test)]
pub(super) const TEST_FLUSH_AT: usize = FLUSH_AT;
