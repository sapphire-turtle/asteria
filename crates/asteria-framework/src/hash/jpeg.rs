//! `media-payload/1` for JPEG: BLAKE3 over every segment except APPn and COM,
//! up to and including EOI. Data after EOI is ignored.

use std::io::{self, Read};

use super::sink::Sink;
use super::{ContentHash, MediaFormat};
use crate::{Error, Result};

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
    let mut sink = Sink::new(&mut hasher);

    let mut soi = [0u8; 2];
    read_exact(r, &mut soi)?;
    if soi != [0xFF, 0xD8] {
        return Err(malformed("missing SOI"));
    }
    sink.push(&soi);

    // A marker already consumed from the entropy-coded data, if any.
    let mut pending: Option<u8> = None;
    loop {
        let marker = match pending.take() {
            Some(m) => m,
            None => {
                if read_u8(r)? != 0xFF {
                    return Err(malformed("expected marker"));
                }
                let mut m = read_u8(r)?;
                while m == 0xFF {
                    m = read_u8(r)?; // fill byte
                }
                m
            }
        };
        match marker {
            0xD9 => {
                sink.push(&[0xFF, 0xD9]);
                break;
            }
            0x00 | 0x01 | 0xD0..=0xD8 => {
                return Err(malformed("unexpected lengthless marker"));
            }
            _ => {}
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
        sink.push(&[0xFF, marker]);
        sink.push(&len);
        sink.push(&body);

        if marker == 0xDA {
            match hash_scan(r, &mut sink)? {
                Some(next) => pending = Some(next),
                None => break, // EOI reached
            }
        }
    }
    sink.flush();
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

/// Hashes entropy-coded data. Returns the next marker byte (its leading 0xFF
/// and any fill bytes consumed, none hashed), or `None` once EOI is hashed.
fn hash_scan(r: &mut impl Read, sink: &mut Sink) -> Result<Option<u8>> {
    loop {
        let b = read_u8(r)?;
        if b != 0xFF {
            sink.push(&[b]);
            continue;
        }
        let mut n = read_u8(r)?;
        while n == 0xFF {
            n = read_u8(r)?; // fill byte
        }
        match n {
            0x00 | 0xD0..=0xD7 => sink.push(&[0xFF, n]),
            0xD9 => {
                sink.push(&[0xFF, 0xD9]);
                return Ok(None);
            }
            _ => return Ok(Some(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOI: &[u8] = &[0xFF, 0xD8];
    const EOI: &[u8] = &[0xFF, 0xD9];
    const DQT: &[u8] = &[0xFF, 0xDB, 0x00, 0x04, 0x01, 0x02];
    const DHT: &[u8] = &[0xFF, 0xC4, 0x00, 0x03, 0x07];
    const SOS: &[u8] = &[0xFF, 0xDA, 0x00, 0x03, 0x09];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    fn hash(bytes: &[u8]) -> Result<ContentHash> {
        jpeg_payload_hash(&mut &bytes[..])
    }

    fn blake(bytes: &[u8]) -> ContentHash {
        ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
    }

    fn is_malformed(r: Result<ContentHash>) -> bool {
        matches!(r, Err(Error::MalformedMedia { .. }))
    }

    #[test]
    fn stuffing_and_rst_are_hashed_and_golden() {
        let scan = [0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xD7, 0x78];
        let input = cat(&[SOI, DQT, SOS, &scan, EOI]);
        let expected = blake(&input);
        assert_eq!(hash(&input).unwrap(), expected);
        assert_eq!(
            expected.to_string(),
            "blake3:6225a1e1c80097bed0488ffb076a57d4e697ad17071177e710f201cee5ad9449",
            "golden digest"
        );
    }

    #[test]
    fn app_and_com_skipped_but_other_segments_hashed() {
        let app = [0xFF, 0xE1, 0x00, 0x04, 0xAA, 0xBB];
        let com = [0xFF, 0xFE, 0x00, 0x03, 0x41];
        let plain = cat(&[SOI, DQT, SOS, &[0x01], EOI]);
        let with = cat(&[SOI, &app, DQT, &com, SOS, &[0x01], EOI]);
        assert_eq!(hash(&plain).unwrap(), hash(&with).unwrap());
        assert_eq!(hash(&plain).unwrap(), blake(&plain));
    }

    #[test]
    fn progressive_second_sos_after_dht() {
        let input = cat(&[SOI, SOS, &[0x01, 0xFF, 0x00], DHT, SOS, &[0x02], EOI]);
        assert_eq!(hash(&input).unwrap(), blake(&input));
    }

    #[test]
    fn data_after_eoi_ignored() {
        let input = cat(&[SOI, DQT, SOS, &[0x01], EOI]);
        let mut padded = input.clone();
        padded.extend_from_slice(&[0xFF, 0xDB, 0xFF]);
        assert_eq!(hash(&padded).unwrap(), blake(&input));
    }

    #[test]
    fn fill_bytes_before_marker_top_level_are_not_hashed() {
        let plain = cat(&[SOI, DQT, SOS, &[0x01], EOI]);
        let filled = cat(&[
            SOI,
            &[0xFF, 0xFF],
            DQT,
            SOS,
            &[0x01],
            &[0xFF, 0xFF, 0xFF],
            EOI,
        ]);
        assert_eq!(hash(&filled).unwrap(), blake(&plain));
    }

    #[test]
    fn fill_bytes_in_scan_are_not_hashed() {
        let plain = cat(&[SOI, SOS, &[0x01], DHT, SOS, &[0x02, 0xFF, 0xD3], EOI]);
        let filled = cat(&[
            SOI,
            SOS,
            &[0x01, 0xFF, 0xFF],
            DHT,
            SOS,
            &[0x02, 0xFF, 0xFF, 0xFF, 0xD3],
            &[0xFF, 0xFF],
            EOI,
        ]);
        assert_eq!(hash(&filled).unwrap(), blake(&plain));
    }

    #[test]
    fn eof_inside_scan_is_malformed() {
        assert!(is_malformed(hash(&cat(&[SOI, SOS, &[0x01, 0x02]]))));
        assert!(is_malformed(hash(&cat(&[SOI, SOS, &[0x01, 0xFF]]))));
        assert!(is_malformed(hash(&cat(&[SOI, SOS, &[0xFF, 0xFF]]))));
    }

    #[test]
    fn hostile_inputs_are_malformed() {
        assert!(is_malformed(hash(&[])));
        assert!(is_malformed(hash(&[0xFF])));
        assert!(is_malformed(hash(&[0x00, 0x00, 0x00])));
        assert!(is_malformed(hash(&[0xFF, 0xD9])));
        assert!(is_malformed(hash(&cat(&[SOI, &[0xFF, 0xDB, 0x00, 0x00]]))));
        assert!(is_malformed(hash(&cat(&[SOI, &[0xFF, 0xDB, 0x00, 0x01]]))));
        assert!(is_malformed(hash(&cat(&[
            SOI,
            &[0xFF, 0xDB, 0xFF, 0xFF, 1]
        ]))));
        assert!(is_malformed(hash(&cat(&[SOI]))));
        assert!(is_malformed(hash(&cat(&[SOI, DQT]))));
        assert!(is_malformed(hash(&cat(&[SOI, &[0x12, 0x34]]))));
    }

    #[test]
    fn lengthless_markers_at_top_level_are_malformed() {
        for m in [0x01u8, 0xD8, 0xD0, 0xD7, 0x00] {
            assert!(
                is_malformed(hash(&cat(&[SOI, &[0xFF, m], DQT, EOI]))),
                "marker {m:#x}"
            );
        }
    }

    #[test]
    fn large_scan_flushes_and_matches_whole_hash() {
        let mut scan = Vec::new();
        for i in 0..(crate::hash::sink::TEST_FLUSH_AT * 3) {
            scan.push((i % 200) as u8);
            if i % 1000 == 0 {
                scan.extend_from_slice(&[0xFF, 0x00]);
            }
        }
        let input = cat(&[SOI, SOS, &scan, EOI]);
        assert_eq!(hash(&input).unwrap(), blake(&input));
    }
}
