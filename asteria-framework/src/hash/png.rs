//! `media-payload/1` for PNG: BLAKE3 over `type + data` of every IHDR, PLTE,
//! IDAT, IEND, tRNS, acTL, fcTL and fdAT chunk, in file order. Lengths, CRCs
//! and all other chunks are excluded; data after IEND is ignored.

use std::io::{self, Read};

use super::sink::{Sink, skip};
use super::{ContentHash, MediaFormat};
use crate::{Error, Result};

const SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";
/// Largest chunk length the PNG specification allows.
const MAX_CHUNK_LEN: u32 = i32::MAX as u32;

fn malformed(reason: &str) -> Error {
    Error::MalformedMedia {
        format: MediaFormat::Png,
        reason: reason.to_string(),
    }
}

fn io_err(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::UnexpectedEof => malformed("unexpected end of file"),
        _ => Error::Io(e),
    }
}

fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> Result<()> {
    r.read_exact(buf).map_err(io_err)
}

fn is_payload(kind: &[u8; 4]) -> bool {
    matches!(
        kind,
        b"IHDR" | b"PLTE" | b"IDAT" | b"IEND" | b"tRNS" | b"acTL" | b"fcTL" | b"fdAT"
    )
}

pub(crate) fn png_payload_hash(r: &mut impl Read) -> Result<ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let mut sink = Sink::new(&mut hasher);

    let mut sig = [0u8; 8];
    read_exact(r, &mut sig)?;
    if sig != SIGNATURE {
        return Err(malformed("bad signature"));
    }

    let mut first = true;
    let mut saw_image = false;
    loop {
        let mut head = [0u8; 8];
        read_exact(r, &mut head)?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        if len > MAX_CHUNK_LEN {
            return Err(malformed("chunk length too large"));
        }
        let kind = [head[4], head[5], head[6], head[7]];
        if first && &kind != b"IHDR" {
            return Err(malformed("IHDR is not the first chunk"));
        }
        first = false;
        saw_image |= matches!(&kind, b"IDAT" | b"fdAT");
        if is_payload(&kind) {
            sink.push(&kind);
            sink.copy_from(r, u64::from(len)).map_err(io_err)?;
        } else {
            skip(r, u64::from(len)).map_err(io_err)?;
        }
        skip(r, 4).map_err(io_err)?; // CRC, not hashed
        if &kind == b"IEND" {
            break;
        }
    }
    if !saw_image {
        return Err(malformed("no image data"));
    }
    sink.flush();
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // CRC is never checked
        out
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        for c in chunks {
            out.extend_from_slice(c);
        }
        out
    }

    fn hash(bytes: &[u8]) -> Result<ContentHash> {
        png_payload_hash(&mut &bytes[..])
    }

    fn blake(bytes: &[u8]) -> ContentHash {
        ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
    }

    fn is_malformed(r: Result<ContentHash>) -> bool {
        matches!(r, Err(Error::MalformedMedia { .. }))
    }

    #[test]
    fn exact_bytes_hashed_for_included_and_excluded_chunks() {
        let input = png(&[
            chunk(b"IHDR", &[1, 2, 3]),
            chunk(b"gAMA", &[9, 9]),
            chunk(b"PLTE", &[4]),
            chunk(b"tEXt", b"Comment\0hi"),
            chunk(b"tRNS", &[5, 6]),
            chunk(b"acTL", &[7]),
            chunk(b"fcTL", &[8]),
            chunk(b"eXIf", b"MM"),
            chunk(b"IDAT", &[10, 11]),
            chunk(b"fdAT", &[12]),
            chunk(b"IDAT", &[]),
            chunk(b"IEND", &[]),
        ]);
        let expected: &[u8] = b"IHDR\x01\x02\x03PLTE\x04tRNS\x05\x06acTL\x07fcTL\x08\
                                IDAT\x0a\x0bfdAT\x0cIDATIEND";
        assert_eq!(hash(&input).unwrap(), blake(expected));
    }

    #[test]
    fn data_after_iend_ignored() {
        let base = png(&[
            chunk(b"IHDR", &[1]),
            chunk(b"IDAT", &[2]),
            chunk(b"IEND", &[]),
        ]);
        let mut padded = base.clone();
        padded.extend_from_slice(&[0xFF; 50]);
        assert_eq!(hash(&padded).unwrap(), hash(&base).unwrap());
    }

    #[test]
    fn large_idat_streams_and_matches_whole_hash() {
        let data: Vec<u8> = (0..super::super::sink::TEST_FLUSH_AT * 3 + 17)
            .map(|i| (i % 251) as u8)
            .collect();
        let input = png(&[
            chunk(b"IHDR", &[1]),
            chunk(b"IDAT", &data),
            chunk(b"IEND", &[]),
        ]);
        let mut expected = b"IHDR\x01IDAT".to_vec();
        expected.extend_from_slice(&data);
        expected.extend_from_slice(b"IEND");
        assert_eq!(hash(&input).unwrap(), blake(&expected));
    }

    #[test]
    fn hostile_inputs_are_malformed() {
        assert!(is_malformed(hash(&[])));
        assert!(is_malformed(hash(&SIGNATURE[..5])));
        assert!(is_malformed(hash(b"NOTAPNGATALL....")));
        assert!(is_malformed(hash(&SIGNATURE))); // no chunks
        // missing IEND
        assert!(is_malformed(hash(&png(&[
            chunk(b"IHDR", &[1]),
            chunk(b"IDAT", &[2])
        ]))));
        // signature + IEND only; IHDR not first; no IDAT/fdAT
        assert!(is_malformed(hash(&png(&[chunk(b"IEND", &[])]))));
        assert!(is_malformed(hash(&png(&[
            chunk(b"IDAT", &[2]),
            chunk(b"IHDR", &[1]),
            chunk(b"IEND", &[])
        ]))));
        assert!(is_malformed(hash(&png(&[
            chunk(b"IHDR", &[1]),
            chunk(b"IEND", &[])
        ]))));
        // truncated at every length of a complete file: this covers cuts in
        // the signature, chunk headers, IHDR data, IDAT data and CRCs
        let full = png(&[
            chunk(b"IHDR", &[1, 2]),
            chunk(b"IDAT", &[3, 4, 5]),
            chunk(b"IEND", &[]),
        ]);
        for cut in 0..full.len() {
            assert!(is_malformed(hash(&full[..cut])), "cut {cut}");
        }
        // length far past EOF, on included and excluded chunks
        for kind in [b"IDAT", b"tEXt"] {
            let mut v = SIGNATURE.to_vec();
            v.extend_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
            v.extend_from_slice(kind);
            v.extend_from_slice(&[1, 2, 3]);
            assert!(is_malformed(hash(&v)));
        }
        // length above the PNG maximum
        let mut v = SIGNATURE.to_vec();
        v.extend_from_slice(&u32::MAX.to_be_bytes());
        v.extend_from_slice(b"IDAT");
        assert!(is_malformed(hash(&v)));
    }
}
