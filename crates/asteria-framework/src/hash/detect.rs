//! Media format detection by magic bytes (never by extension).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::MediaFormat;
use super::id3::id3v2_total_len;
use crate::Result;

/// Header-only fast path: detects the format from the first 16 bytes. A FLAC stream behind an
/// ID3v2 tag is reported as `Mp3` here, and a bare MPEG frame header is accepted on its own;
/// use [`detect_format_file`] to see through the tag and to also require a second frame header.
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

/// Length in bytes of the MPEG audio frame that starts with `head`, or `None` for free-format
/// streams (bitrate index 0) and invalid headers.
fn mpeg_frame_len(head: &[u8]) -> Option<u64> {
    if !is_mpeg_audio_header(head) {
        return None;
    }
    let (b1, b2) = (head[1], head[2]);
    let version = (b1 >> 3) & 0b11; // 11 = MPEG-1, 10 = MPEG-2, 00 = MPEG-2.5
    let layer = (b1 >> 1) & 0b11; // 11 = I, 10 = II, 01 = III
    let bitrate_idx = usize::from(b2 >> 4);
    let sr_idx = usize::from((b2 >> 2) & 0b11);
    let padding = u64::from((b2 >> 1) & 1);
    if bitrate_idx == 0 {
        return None;
    }
    const V1_L1: [u32; 14] = [
        32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const V1_L2: [u32; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const V1_L3: [u32; 14] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2_L1: [u32; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const V2_L23: [u32; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let table = match (version, layer) {
        (0b11, 0b11) => &V1_L1,
        (0b11, 0b10) => &V1_L2,
        (0b11, _) => &V1_L3,
        (_, 0b11) => &V2_L1,
        _ => &V2_L23,
    };
    let sample_rate = match version {
        0b11 => [44100u64, 48000, 32000][sr_idx],
        0b10 => [22050, 24000, 16000][sr_idx],
        _ => [11025, 12000, 8000][sr_idx],
    };
    let bitrate = u64::from(table[bitrate_idx - 1]) * 1000;
    Some(match layer {
        0b11 => (12 * bitrate / sample_rate + padding) * 4,
        _ if layer == 0b01 && version != 0b11 => 72 * bitrate / sample_rate + padding,
        _ => 144 * bitrate / sample_rate + padding,
    })
}

/// A bare MPEG frame header counts as MP3 only if a second valid header sits one frame later.
/// A file that ends right after the first frame (before the second header) is accepted on the
/// first alone; a file too short to hold even the first frame is not.
fn confirm_second_frame(file: &mut File, head: &[u8]) -> Result<bool> {
    let Some(len) = mpeg_frame_len(head) else {
        return Ok(true);
    };
    let file_len = file.metadata()?.len();
    if file_len < len {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len))?;
    let mut next = [0u8; 4];
    let n = read_up_to(file, &mut next)?;
    Ok(n < 4 || is_mpeg_audio_header(&next))
}

/// Like [`detect_format`], but also recognises FLAC after an ID3v2 tag and requires a second
/// valid MPEG frame header for MP3 files that do not start with an ID3v2 tag.
pub fn detect_format_file(path: &Path) -> Result<MediaFormat> {
    let mut file = File::open(path)?;
    let mut head = [0u8; 16];
    let n = read_up_to(&mut file, &mut head)?;
    let head = &head[..n];
    let format = detect_format(head);
    if format == MediaFormat::Mp3
        && !head.starts_with(b"ID3")
        && !confirm_second_frame(&mut file, head)?
    {
        return Ok(MediaFormat::Unknown);
    }
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
