//! `media-payload/1` for WebP: BLAKE3 over `FourCC + data` of every `VP8 `,
//! `VP8L`, `ALPH`, `ANIM` and `ANMF` chunk, in file order. VP8X (it carries
//! metadata-presence flags), ICCP, EXIF, XMP, unknown chunks, sizes and pad
//! bytes are excluded. Bytes beyond the RIFF size are ignored.

use std::io::{self, Read};

use super::sink::{Sink, skip};
use super::{ContentHash, MediaFormat};
use crate::{Error, Result};

fn malformed(reason: &str) -> Error {
    Error::MalformedMedia {
        format: MediaFormat::WebP,
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

fn is_payload(fourcc: &[u8; 4]) -> bool {
    matches!(fourcc, b"VP8 " | b"VP8L" | b"ALPH" | b"ANIM" | b"ANMF")
}

pub(crate) fn webp_payload_hash(r: &mut impl Read) -> Result<ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let mut sink = Sink::new(&mut hasher);

    let mut head = [0u8; 12];
    read_exact(r, &mut head)?;
    if &head[..4] != b"RIFF" || &head[8..] != b"WEBP" {
        return Err(malformed("bad RIFF/WEBP header"));
    }
    let riff_size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    // Bytes of chunk data still inside the RIFF container (after "WEBP").
    let mut left = u64::from(
        riff_size
            .checked_sub(4)
            .ok_or_else(|| malformed("RIFF size too small"))?,
    );

    let mut saw_image = false;
    while left > 0 {
        if left < 8 {
            return Err(malformed("truncated chunk header"));
        }
        let mut ch = [0u8; 8];
        read_exact(r, &mut ch)?;
        let fourcc = [ch[0], ch[1], ch[2], ch[3]];
        let len = u64::from(u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]));
        let padded = len + (len & 1);
        left -= 8;
        if padded > left {
            return Err(malformed("chunk exceeds RIFF size"));
        }
        if is_payload(&fourcc) {
            saw_image |= matches!(&fourcc, b"VP8 " | b"VP8L" | b"ANMF");
            sink.push(&fourcc);
            sink.copy_from(r, len).map_err(io_err)?;
        } else {
            skip(r, len).map_err(io_err)?;
        }
        skip(r, padded - len).map_err(io_err)?; // pad byte, not hashed
        left -= padded;
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

    fn chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = fourcc.to_vec();
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0xEE); // pad, must not be hashed
        }
        out
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&body);
        out
    }

    fn hash(bytes: &[u8]) -> Result<ContentHash> {
        webp_payload_hash(&mut &bytes[..])
    }

    fn blake(bytes: &[u8]) -> ContentHash {
        ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
    }

    fn is_malformed(r: Result<ContentHash>) -> bool {
        matches!(r, Err(Error::MalformedMedia { .. }))
    }

    #[test]
    fn exact_bytes_hashed_for_included_and_excluded_chunks() {
        let input = riff(&[
            chunk(b"VP8X", &[0x08; 10]),
            chunk(b"ICCP", &[1]),
            chunk(b"ANIM", &[1, 2, 3, 4, 5, 6]),
            chunk(b"EXIF", b"Exif\0"),
            chunk(b"ALPH", &[7, 8, 9]), // odd: pad excluded
            chunk(b"XMP ", b"xmp"),
            chunk(b"ANMF", &[10]),
            chunk(b"JUNK", &[0; 4]),
            chunk(b"VP8 ", &[11, 12]),
            chunk(b"VP8L", &[13]),
        ]);
        let expected: &[u8] =
            b"ANIM\x01\x02\x03\x04\x05\x06ALPH\x07\x08\x09ANMF\x0aVP8 \x0b\x0cVP8L\x0d";
        assert_eq!(hash(&input).unwrap(), blake(expected));
    }

    #[test]
    fn bytes_after_riff_end_ignored() {
        let base = riff(&[chunk(b"VP8L", &[1, 2])]);
        let mut padded = base.clone();
        padded.extend_from_slice(&[0xFF; 30]);
        assert_eq!(hash(&padded).unwrap(), hash(&base).unwrap());
        assert_eq!(hash(&base).unwrap(), blake(b"VP8L\x01\x02"));
    }

    #[test]
    fn large_chunk_streams_and_matches_whole_hash() {
        let data: Vec<u8> = (0..super::super::sink::TEST_FLUSH_AT * 3 + 17)
            .map(|i| (i % 251) as u8)
            .collect();
        let input = riff(&[chunk(b"VP8 ", &data)]);
        let mut expected = b"VP8 ".to_vec();
        expected.extend_from_slice(&data);
        assert_eq!(hash(&input).unwrap(), blake(&expected));
    }

    #[test]
    fn hostile_inputs_are_malformed() {
        assert!(is_malformed(hash(&[])));
        assert!(is_malformed(hash(b"RIFF")));
        assert!(is_malformed(hash(b"RIFX\x04\0\0\0WEBP")));
        assert!(is_malformed(hash(b"RIFF\x04\0\0\0WAVE")));
        assert!(is_malformed(hash(b"RIFF\x04\0\0\0WEBP"))); // no image
        assert!(is_malformed(hash(b"RIFF\x03\0\0\0WEBP"))); // size < 4
        assert!(is_malformed(hash(b"RIFF\0\0\0\0WEBP")));
        // RIFF size smaller than the chunks it holds
        let mut small = riff(&[chunk(b"VP8L", &[1, 2, 3, 4])]);
        small[4..8].copy_from_slice(&10u32.to_le_bytes());
        assert!(is_malformed(hash(&small)));
        // RIFF size larger than the file
        let mut big = riff(&[chunk(b"VP8L", &[1, 2])]);
        big[4..8].copy_from_slice(&1000u32.to_le_bytes());
        assert!(is_malformed(hash(&big)));
        // chunk length near u32::MAX, with a huge RIFF size too
        for fourcc in [b"VP8 ", b"EXIF"] {
            let mut v = b"RIFF\xff\xff\xff\xffWEBP".to_vec();
            v.extend_from_slice(fourcc);
            v.extend_from_slice(&u32::MAX.to_le_bytes());
            v.extend_from_slice(&[1, 2, 3]);
            assert!(is_malformed(hash(&v)));
        }
        // leftover bytes too short for a chunk header
        let mut v = riff(&[chunk(b"VP8L", &[1, 2])]);
        v.extend_from_slice(&[0; 3]);
        let size = u32::try_from(v.len() - 8).unwrap();
        v[4..8].copy_from_slice(&size.to_le_bytes());
        assert!(is_malformed(hash(&v)));
        // truncated at every length
        let full = riff(&[chunk(b"VP8X", &[0; 10]), chunk(b"VP8L", &[1, 2, 3])]);
        for cut in 0..full.len() {
            assert!(is_malformed(hash(&full[..cut])), "cut {cut}");
        }
    }
}
