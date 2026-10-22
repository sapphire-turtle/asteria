//! `media-payload/1` for FLAC: BLAKE3 over every byte after the metadata
//! blocks (all of them, STREAMINFO included) up to the start of any trailing
//! ID3v1 / APEv2 tag (see `trailer`) or the end of the file. A leading ID3v2
//! tag is skipped. At least one audio byte is required.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use super::id3::id3v2_total_len;
use super::sink::{Sink, skip};
use super::trailer::audio_end;
use super::{ContentHash, MediaFormat};
use crate::{Error, Result};

fn malformed(reason: &str) -> Error {
    Error::MalformedMedia {
        format: MediaFormat::Flac,
        reason: reason.to_string(),
    }
}

fn io_err(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::UnexpectedEof => malformed("unexpected end of file"),
        _ => Error::Io(e),
    }
}

pub(crate) fn flac_payload_hash(f: &mut File) -> Result<ContentHash> {
    flac_hash(f)
}

fn flac_hash<R: Read + Seek>(r: &mut R) -> Result<ContentHash> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).map_err(io_err)?;
    if magic.starts_with(b"ID3") {
        let mut head = [0u8; 10];
        head[..4].copy_from_slice(&magic);
        r.read_exact(&mut head[4..]).map_err(io_err)?;
        let total = id3v2_total_len(&head).ok_or_else(|| malformed("bad ID3v2 header"))?;
        skip(r, total - 10).map_err(io_err)?;
        r.read_exact(&mut magic).map_err(io_err)?;
    }
    if &magic != b"fLaC" {
        return Err(malformed("missing fLaC marker"));
    }

    loop {
        let mut head = [0u8; 4];
        r.read_exact(&mut head).map_err(io_err)?;
        let len = u32::from_be_bytes([0, head[1], head[2], head[3]]);
        skip(r, u64::from(len)).map_err(io_err)?;
        if head[0] & 0x80 != 0 {
            break;
        }
    }

    let start = r.stream_position()?;
    let end = audio_end(r, start, len)
        .map_err(io_err)?
        .map_err(malformed)?;
    if end <= start {
        return Err(malformed("no audio frames"));
    }
    r.seek(SeekFrom::Start(start))?;
    let mut hasher = blake3::Hasher::new();
    let mut sink = Sink::new(&mut hasher);
    sink.copy_from(r, end - start).map_err(io_err)?;
    sink.flush();
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::super::trailer::test_util::{ape, id3v1};
    use super::*;

    fn block(last: bool, kind: u8, data: &[u8]) -> Vec<u8> {
        let len = u32::try_from(data.len()).unwrap().to_be_bytes();
        let mut out = vec![kind | if last { 0x80 } else { 0 }, len[1], len[2], len[3]];
        out.extend_from_slice(data);
        out
    }

    fn hash(bytes: &[u8]) -> Result<ContentHash> {
        flac_hash(&mut std::io::Cursor::new(bytes))
    }

    fn blake(bytes: &[u8]) -> ContentHash {
        ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
    }

    fn is_malformed(r: Result<ContentHash>) -> bool {
        matches!(r, Err(Error::MalformedMedia { .. }))
    }

    #[test]
    fn exact_bytes_hashed_are_the_frames_only() {
        let mut input = b"ID3\x04\x00\x10\x00\x00\x00\x03abc".to_vec(); // 3 body bytes
        input.extend_from_slice(&[0; 10]); // v2.4 footer
        input.extend_from_slice(b"fLaC");
        input.extend(block(false, 0, &[1, 2, 3])); // STREAMINFO
        input.extend(block(false, 4, b"vendor")); // VORBIS_COMMENT
        input.extend(block(false, 6, &[9; 5])); // PICTURE
        input.extend(block(true, 1, &[])); // empty PADDING, last
        input.extend_from_slice(&[0xFF, 0xF8, 0xAA, 0xBB]);
        assert_eq!(hash(&input).unwrap(), blake(&[0xFF, 0xF8, 0xAA, 0xBB]));
    }

    #[test]
    fn trailing_ape_and_id3v1_are_excluded() {
        let audio = [0xFF, 0xF8, 0x01, 0x02, 0x03];
        let mut input = b"fLaC".to_vec();
        input.extend(block(true, 0, &[1, 2]));
        input.extend_from_slice(&audio);
        input.extend(ape(9, true));
        input.extend(id3v1());
        assert_eq!(hash(&input).unwrap(), blake(&audio));
    }

    #[test]
    fn id3v23_footer_bit_is_not_a_footer() {
        let mut input = b"ID3\x03\x00\x10\x00\x00\x00\x03abc".to_vec();
        input.extend_from_slice(b"fLaC");
        input.extend(block(true, 0, &[1]));
        input.extend_from_slice(&[7, 8]);
        assert_eq!(hash(&input).unwrap(), blake(&[7, 8]));
    }

    #[test]
    fn large_audio_streams() {
        let audio: Vec<u8> = (0..super::super::sink::TEST_FLUSH_AT * 3 + 5)
            .map(|i| (i % 251) as u8)
            .collect();
        let mut input = b"fLaC".to_vec();
        input.extend(block(true, 0, &[1]));
        input.extend_from_slice(&audio);
        assert_eq!(hash(&input).unwrap(), blake(&audio));
    }

    #[test]
    fn hostile_inputs_are_malformed() {
        assert!(is_malformed(hash(&[])));
        assert!(is_malformed(hash(b"fLa")));
        assert!(is_malformed(hash(b"OggS\0\0\0\0")));
        // block length past EOF
        assert!(is_malformed(hash(b"fLaC\x80\xFF\xFF\xFF\x01\x02")));
        // no audio frames after the last block
        let mut v = b"fLaC".to_vec();
        v.extend(block(true, 0, &[1, 2]));
        assert!(is_malformed(hash(&v)));
        // last-block flag never set
        let mut v = b"fLaC".to_vec();
        v.extend(block(false, 0, &[1, 2]));
        assert!(is_malformed(hash(&v)));
        // ID3v2 length past EOF; bad syncsafe size
        assert!(is_malformed(hash(b"ID3\x04\x00\x00\x00\x00\x7F\x7FfLaC")));
        assert!(is_malformed(hash(b"ID3\x04\x00\x00\x80\x00\x00\x00fLaC")));
        // trailer that consumes all audio
        let mut v = b"fLaC".to_vec();
        v.extend(block(true, 0, &[1]));
        v.extend(id3v1());
        assert!(is_malformed(hash(&v)));
        // every truncation shorter than the first audio byte is malformed
        let mut full = b"fLaC".to_vec();
        full.extend(block(true, 0, &[1, 2]));
        full.push(9);
        for cut in 0..full.len() {
            assert!(is_malformed(hash(&full[..cut])), "cut {cut}");
        }
        assert_eq!(hash(&full).unwrap(), blake(&[9]));
    }
}
