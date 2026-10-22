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
