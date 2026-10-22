//! Media format detection by magic bytes (never by extension).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::MediaFormat;
use super::id3::id3v2_total_len;
use crate::Result;

/// Detects the format from the first 16 bytes. A FLAC stream behind an ID3v2
/// tag is reported as `Mp3` here; use [`detect_format_file`] to see through it.
pub fn detect_format(head: &[u8]) -> MediaFormat {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        MediaFormat::Jpeg
    } else if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        MediaFormat::Png
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        MediaFormat::WebP
    } else if head.starts_with(b"fLaC") {
        MediaFormat::Flac
    } else if head.starts_with(b"ID3") || is_mpeg_audio_header(head) {
        MediaFormat::Mp3
    } else {
        MediaFormat::Unknown
    }
}

/// An MPEG audio frame header: the 11-bit sync word plus version, layer, bitrate index and
/// sample-rate index that are not reserved. Layer `00` is reserved, which also leaves AAC ADTS
/// streams unmatched.
fn is_mpeg_audio_header(head: &[u8]) -> bool {
    let [0xFF, b1, b2, ..] = *head else {
        return false;
    };
    let sync = b1 & 0xE0 == 0xE0;
    let version = (b1 >> 3) & 0b11;
    let layer = (b1 >> 1) & 0b11;
    let bitrate = b2 >> 4;
    let sample_rate = (b2 >> 2) & 0b11;
    sync && version != 0b01 && layer != 0b00 && bitrate != 0b1111 && sample_rate != 0b11
}

/// Like [`detect_format`], but also recognises FLAC after an ID3v2 tag.
pub fn detect_format_file(path: &Path) -> Result<MediaFormat> {
    let mut file = File::open(path)?;
    let mut head = [0u8; 16];
    let n = read_up_to(&mut file, &mut head)?;
    let head = &head[..n];
    let format = detect_format(head);
    if format == MediaFormat::Mp3 && head.starts_with(b"ID3") && head.len() >= 10 {
        let id3_head: &[u8; 10] = head[..10].try_into().expect("length checked");
        if let Some(total) = id3v2_total_len(id3_head) {
            file.seek(SeekFrom::Start(total))?;
            let mut magic = [0u8; 4];
            if read_up_to(&mut file, &mut magic)? == 4 && &magic == b"fLaC" {
                return Ok(MediaFormat::Flac);
            }
        }
    }
    Ok(format)
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match r.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}
