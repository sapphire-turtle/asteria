//! Helpers shared by several test binaries.
#![allow(dead_code)]

use std::path::PathBuf;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
    let len = u16::try_from(body.len() + 2).expect("segment too long");
    let mut out = vec![0xFF, marker];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn insert_after_soi(jpeg: &[u8], seg: &[u8]) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "not a JPEG");
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(seg);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// Inserts an APP1 segment carrying `payload` right after SOI.
pub fn jpeg_insert_app1(jpeg: &[u8], payload: &[u8]) -> Vec<u8> {
    insert_after_soi(jpeg, &segment(0xE1, payload))
}

/// Inserts a COM segment carrying `text` right after SOI.
pub fn jpeg_insert_com(jpeg: &[u8], text: &str) -> Vec<u8> {
    insert_after_soi(jpeg, &segment(0xFE, text.as_bytes()))
}

/// Inserts a PNG chunk (with a correct CRC) right after IHDR.
pub fn png_insert_chunk(png: &[u8], kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    assert_eq!(&png[12..16], b"IHDR", "IHDR must come first");
    let ihdr_len = u32::from_be_bytes(png[8..12].try_into().unwrap()) as usize;
    let after_ihdr = 8 + 4 + 4 + ihdr_len + 4;

    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);

    let mut out = png[..after_ihdr].to_vec();
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
    out.extend_from_slice(&png[after_ihdr..]);
    out
}

fn riff_chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = fourcc.to_vec();
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// Adds an EXIF chunk to a WebP, creating a VP8X chunk (EXIF flag set,
/// canvas size filled in) when absent, and fixes the RIFF size.
pub fn webp_add_exif(webp: &[u8], exif: &[u8]) -> Vec<u8> {
    assert!(
        &webp[..4] == b"RIFF" && &webp[8..12] == b"WEBP",
        "not a WebP"
    );
    let mut chunks = webp[12..].to_vec();
    if &chunks[..4] == b"VP8X" {
        chunks[8] |= 0x08;
    } else {
        let (w, h) = match &chunks[..4] {
            b"VP8L" => {
                let b = &chunks[9..13]; // after fourcc, size, 0x2f signature
                let bits = u32::from_le_bytes(b.try_into().unwrap());
                ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
            }
            b"VP8 " => {
                let b = &chunks[14..18]; // after 10-byte header: start code, w, h
                let w = u32::from(u16::from_le_bytes([b[0], b[1]]) & 0x3FFF);
                let h = u32::from(u16::from_le_bytes([b[2], b[3]]) & 0x3FFF);
                (w, h)
            }
            other => panic!("unsupported WebP chunk {other:?}"),
        };
        let mut vp8x = vec![0x08, 0, 0, 0];
        vp8x.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        vp8x.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        let mut with = riff_chunk(b"VP8X", &vp8x);
        with.extend_from_slice(&chunks);
        chunks = with;
    }
    chunks.extend_from_slice(&riff_chunk(b"EXIF", exif));

    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&u32::try_from(chunks.len() + 4).unwrap().to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&chunks);
    out
}
