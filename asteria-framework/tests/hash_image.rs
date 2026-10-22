mod common;

use asteria_framework::hash::{HashScope, content_hash};
use common::{fixture, jpeg_insert_app1, jpeg_insert_com};

fn hash_bytes(bytes: &[u8]) -> asteria_framework::hash::HashOutcome {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.jpg");
    std::fs::write(&p, bytes).unwrap();
    content_hash(&p).unwrap()
}

fn red() -> Vec<u8> {
    std::fs::read(fixture("red.jpg")).unwrap()
}

#[test]
fn jpeg_hash_ignores_app_and_com_segments() {
    let red = red();
    let plain = hash_bytes(&red);
    let app1 = hash_bytes(&jpeg_insert_app1(&red, b"Exif\0\0some exif data"));
    let com = hash_bytes(&jpeg_insert_com(&red, "hi"));
    assert_eq!(plain.scope, HashScope::MediaPayload1);
    assert_eq!(plain.fallback_reason, None);
    assert_eq!(app1.scope, HashScope::MediaPayload1);
    assert_eq!(com.scope, HashScope::MediaPayload1);
    assert_eq!(plain.hash, app1.hash);
    assert_eq!(plain.hash, com.hash);
}

#[test]
fn jpeg_hash_differs_for_different_pixels() {
    let blue = std::fs::read(fixture("blue.jpg")).unwrap();
    assert_ne!(hash_bytes(&red()).hash, hash_bytes(&blue).hash);
}

#[test]
fn jpeg_hash_ignores_trailing_bytes_after_eoi() {
    let mut padded = red();
    padded.extend_from_slice(&[0xAB; 100]);
    let out = hash_bytes(&padded);
    assert_eq!(out.scope, HashScope::MediaPayload1);
    assert_eq!(out.hash, hash_bytes(&red()).hash);
}

#[test]
fn truncated_jpeg_falls_back_without_panic() {
    let red = red();
    let out = hash_bytes(&red[..red.len() / 2]);
    assert_eq!(out.scope, HashScope::File1);
    assert!(out.fallback_reason.is_some());
}
