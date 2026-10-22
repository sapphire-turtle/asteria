//! `media-payload/1` for MP3: BLAKE3 over the file minus a leading ID3v2 tag
//! and trailing ID3v1 / APEv2 tags (see `trailer`).

use std::io::{self, Read, Seek, SeekFrom};

use super::id3::id3v2_total_len;
use super::sink::Sink;
use super::trailer::audio_end;
use super::{ContentHash, MediaFormat};
use crate::{Error, Result};

fn malformed(reason: &str) -> Error {
    Error::MalformedMedia {
        format: MediaFormat::Mp3,
        reason: reason.to_string(),
    }
}

fn io_err(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::UnexpectedEof => malformed("unexpected end of file"),
        _ => Error::Io(e),
    }
}

/// Takes a `File` (or anything seekable): trailing tags are cut from the end.
pub(crate) fn mp3_payload_hash<R: Read + Seek>(f: &mut R) -> Result<ContentHash> {
    let len = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;

    let mut start = 0u64;
    let mut head = [0u8; 10];
    if len >= 10 {
        f.read_exact(&mut head).map_err(io_err)?;
        if head.starts_with(b"ID3") {
            start = id3v2_total_len(&head).ok_or_else(|| malformed("bad ID3v2 header"))?;
        }
    }
    if start > len {
        return Err(malformed("ID3v2 tag extends past end of file"));
    }

    let end = audio_end(f, start, len)
        .map_err(io_err)?
        .map_err(malformed)?;
    if end <= start {
        return Err(malformed("no audio data"));
    }

    f.seek(SeekFrom::Start(start))?;
    let mut hasher = blake3::Hasher::new();
    let mut sink = Sink::new(&mut hasher);
    sink.copy_from(f, end - start).map_err(io_err)?;
    sink.flush();
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use super::super::trailer::test_util::{ape, id3v1};

    fn hash(bytes: &[u8]) -> Result<ContentHash> {
        mp3_payload_hash(&mut Cursor::new(bytes))
    }

    fn blake(bytes: &[u8]) -> ContentHash {
        ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
    }

    fn is_malformed(r: Result<ContentHash>) -> bool {
        matches!(r, Err(Error::MalformedMedia { .. }))
    }

    const AUDIO: [u8; 6] = [0xFF, 0xFB, 0x90, 0x00, 0x11, 0x22];

    #[test]
    fn exact_bytes_hashed_exclude_id3v2_ape_and_id3v1() {
        let mut input = b"ID3\x04\x00\x10\x00\x00\x00\x04abcd".to_vec();
        input.extend_from_slice(&[0; 10]); // v2.4 footer
        input.extend_from_slice(&AUDIO);
        input.extend(ape(7, true));
        input.extend(id3v1());
        assert_eq!(hash(&input).unwrap(), blake(&AUDIO));
    }

    #[test]
    fn ape_without_header_and_without_id3v1() {
        let mut input = AUDIO.to_vec();
        input.extend(ape(5, false));
        assert_eq!(hash(&input).unwrap(), blake(&AUDIO));
    }

    #[test]
    fn id3v23_footer_bit_is_not_a_footer() {
        let mut input = b"ID3\x03\x00\x10\x00\x00\x00\x02ab".to_vec();
        input.extend_from_slice(&AUDIO);
        assert_eq!(hash(&input).unwrap(), blake(&AUDIO));
    }

    #[test]
    fn ape_header_at_eof_is_not_a_footer() {
        // 32 bytes with the is-header flag (bit 29) set must not be stripped.
        let mut hdr = ape(0, false);
        hdr[20..24].copy_from_slice(&(1u32 << 29).to_le_bytes());
        let mut input = AUDIO.to_vec();
        input.extend_from_slice(&hdr);
        assert_eq!(hash(&input).unwrap(), blake(&input));
    }

    #[test]
    fn no_tags_hashes_whole_file() {
        assert_eq!(hash(&AUDIO).unwrap(), blake(&AUDIO));
    }

    #[test]
    fn large_audio_streams() {
        let audio: Vec<u8> = (0..super::super::sink::TEST_FLUSH_AT * 3 + 5)
            .map(|i| (i % 251) as u8)
            .collect();
        let mut input = audio.clone();
        input.extend(id3v1());
        assert_eq!(hash(&input).unwrap(), blake(&audio));
    }

    #[test]
    fn hostile_inputs_are_malformed() {
        assert!(is_malformed(hash(&[])));
        // ID3v2 length past EOF, exactly to EOF, bad syncsafe size
        assert!(is_malformed(hash(b"ID3\x04\x00\x00\x00\x00\x7F\x7Fabc")));
        assert!(is_malformed(hash(b"ID3\x04\x00\x00\x00\x00\x00\x02ab")));
        assert!(is_malformed(hash(b"ID3\x04\x00\x00\x80\x00\x00\x00abc")));
        // tags that consume the whole file
        assert!(is_malformed(hash(&id3v1())));
        assert!(is_malformed(hash(&ape(4, true))));
        assert!(is_malformed(hash(&ape(4, false))));
        let mut v = ape(4, true);
        v.extend(id3v1());
        assert!(is_malformed(hash(&v)));
        // ID3v2 followed only by ID3v1
        let mut v = b"ID3\x03\x00\x00\x00\x00\x00\x01a".to_vec();
        v.extend(id3v1());
        assert!(is_malformed(hash(&v)));
        // APE size claims more than the file holds
        let mut v = AUDIO.to_vec();
        let mut tag = ape(0, false);
        tag[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        v.extend(tag);
        assert!(is_malformed(hash(&v)));
        // APE size below its own footer
        let mut v = AUDIO.to_vec();
        let mut tag = ape(0, false);
        tag[12..16].copy_from_slice(&3u32.to_le_bytes());
        v.extend(tag);
        assert!(is_malformed(hash(&v)));
    }
}
