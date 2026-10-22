//! `media-payload/1` for JPEG: BLAKE3 over every segment except APPn and COM,
//! up to and including EOI. Data after EOI is ignored.

use std::io::{self, Read};

use super::{ContentHash, MediaFormat};
use crate::{Error, Result};

const FLUSH_AT: usize = 64 * 1024;

fn malformed(reason: &str) -> Error {
    Error::MalformedMedia {
        format: MediaFormat::Jpeg,
        reason: reason.to_string(),
    }
}

fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> Result<()> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => malformed("unexpected end of file"),
        _ => Error::Io(e),
    })
}

fn read_u8(r: &mut impl Read) -> Result<u8> {
    let mut b = [0u8; 1];
    read_exact(r, &mut b)?;
    Ok(b[0])
}

pub(crate) fn jpeg_payload_hash(r: &mut impl Read) -> Result<ContentHash> {
    let mut hasher = blake3::Hasher::new();

    let mut soi = [0u8; 2];
    read_exact(r, &mut soi)?;
    if soi != [0xFF, 0xD8] {
        return Err(malformed("missing SOI"));
    }
    hasher.update(&soi);

    // A marker already consumed from the entropy-coded data, if any.
    let mut pending: Option<u8> = None;
    loop {
        let marker = match pending.take() {
            Some(m) => m,
            None => {
                if read_u8(r)? != 0xFF {
                    return Err(malformed("expected marker"));
                }
                read_u8(r)?
            }
        };
        if marker == 0xD9 {
            hasher.update(&[0xFF, 0xD9]);
            break;
        }
        let mut len = [0u8; 2];
        read_exact(r, &mut len)?;
        let body_len = usize::from(u16::from_be_bytes(len))
            .checked_sub(2)
            .ok_or_else(|| malformed("invalid segment length"))?;
        let mut body = vec![0u8; body_len];
        read_exact(r, &mut body)?;

        if matches!(marker, 0xE0..=0xEF | 0xFE) {
            continue; // APPn and COM carry metadata only
        }
        hasher.update(&[0xFF, marker]);
        hasher.update(&len);
        hasher.update(&body);

        if marker == 0xDA {
            match hash_scan(r, &mut hasher)? {
                Some(next) => pending = Some(next),
                None => break, // EOI reached
            }
        }
    }
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

/// Hashes entropy-coded data. Returns the next marker byte (its leading 0xFF
/// consumed but not hashed), or `None` once EOI has been hashed.
fn hash_scan(r: &mut impl Read, hasher: &mut blake3::Hasher) -> Result<Option<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(FLUSH_AT);
    let mut b = read_u8(r)?;
    let result = loop {
        if b != 0xFF {
            buf.push(b);
            if buf.len() >= FLUSH_AT {
                hasher.update(&buf);
                buf.clear();
            }
            b = read_u8(r)?;
            continue;
        }
        match read_u8(r)? {
            n @ (0x00 | 0xD0..=0xD7) => {
                buf.extend_from_slice(&[0xFF, n]);
                b = read_u8(r)?;
            }
            0xD9 => {
                buf.extend_from_slice(&[0xFF, 0xD9]);
                break None;
            }
            0xFF => {
                // Fill byte before a marker.
                buf.push(0xFF);
                b = 0xFF;
            }
            n => break Some(n),
        }
    };
    hasher.update(&buf);
    Ok(result)
}
