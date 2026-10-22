//! Trailing tag detection shared by the MP3 and FLAC hashers.

use std::io::{self, Read, Seek, SeekFrom};

const ID3V1_LEN: u64 = 128;
const APE_FOOTER_LEN: u64 = 32;
const APE_IS_HEADER: u32 = 1 << 29;
const APE_HAS_HEADER: u32 = 1 << 31;

/// Finds where the audio ends: strips a trailing ID3v1 tag (last 128 bytes
/// starting `TAG`), then an APEv2 tag (a footer `APETAGEX` with the is-header
/// flag clear, plus the 32-byte header if the footer says there is one).
/// Only the region `start..len` is considered. Returns `Ok(Err(reason))` if an
/// APE footer claims a size that does not fit.
pub(super) fn audio_end<R: Read + Seek>(
    f: &mut R,
    start: u64,
    len: u64,
) -> io::Result<std::result::Result<u64, &'static str>> {
    let mut end = len;
    if end.saturating_sub(start) >= ID3V1_LEN {
        let mut tag = [0u8; 3];
        f.seek(SeekFrom::Start(end - ID3V1_LEN))?;
        f.read_exact(&mut tag)?;
        if &tag == b"TAG" {
            end -= ID3V1_LEN;
        }
    }
    if end.saturating_sub(start) >= APE_FOOTER_LEN {
        let mut footer = [0u8; APE_FOOTER_LEN as usize];
        f.seek(SeekFrom::Start(end - APE_FOOTER_LEN))?;
        f.read_exact(&mut footer)?;
        let flags = u32::from_le_bytes([footer[20], footer[21], footer[22], footer[23]]);
        if footer.starts_with(b"APETAGEX") && flags & APE_IS_HEADER == 0 {
            // Tag size counts the items and the footer, not the header.
            let size = u64::from(u32::from_le_bytes([
                footer[12], footer[13], footer[14], footer[15],
            ]));
            let header = if flags & APE_HAS_HEADER != 0 {
                APE_FOOTER_LEN
            } else {
                0
            };
            if size < APE_FOOTER_LEN || size + header > end - start {
                return Ok(Err("APE tag size out of range"));
            }
            end -= size + header;
        }
    }
    Ok(Ok(end))
}

#[cfg(test)]
pub(super) mod test_util {
    pub(in crate::hash) fn id3v1() -> Vec<u8> {
        let mut v = b"TAG".to_vec();
        v.resize(128, b'x');
        v
    }

    /// APEv2 tag: optional header, `body_len` item bytes, footer.
    pub(in crate::hash) fn ape(body_len: usize, with_header: bool) -> Vec<u8> {
        let size = u32::try_from(body_len + 32).unwrap();
        let flags: u32 = if with_header { 1 << 31 } else { 0 };
        let mut out = Vec::new();
        if with_header {
            out.extend_from_slice(b"APETAGEX");
            out.extend_from_slice(&[0xAA; 24]);
        }
        out.extend(std::iter::repeat_n(0xCC, body_len));
        out.extend_from_slice(b"APETAGEX");
        out.extend_from_slice(&2000u32.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        out
    }
}
