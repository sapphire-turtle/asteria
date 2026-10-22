mod common;

use asteria_framework::hash::{HashScope, content_hash};
use common::{fixture, jpeg_insert_app1, jpeg_insert_com, png_insert_chunk, webp_add_exif};

fn hash_named(name: &str, bytes: &[u8]) -> asteria_framework::hash::HashOutcome {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::fs::write(&p, bytes).unwrap();
    content_hash(&p).unwrap()
}

fn hash_bytes(bytes: &[u8]) -> asteria_framework::hash::HashOutcome {
    hash_named("x.jpg", bytes)
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

#[test]
fn red_jpg_payload_hash_is_pinned() {
    // Persisted in sidecars: changing this value breaks re-linking.
    let out = hash_bytes(&red());
    assert_eq!(out.scope, HashScope::MediaPayload1);
    assert_eq!(
        out.hash.to_string(),
        "blake3:47804b280c53cf08cab26f0c7c9e2d07f4e0ed089402511cd81d73f18f9cfeec"
    );
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).unwrap()
}

#[test]
fn png_hash_ignores_text_and_exif_chunks() {
    let red = read("red.png");
    let plain = hash_named("x.png", &red);
    let text = hash_named("x.png", &png_insert_chunk(&red, b"tEXt", b"Comment\0hi"));
    let exif = hash_named("x.png", &png_insert_chunk(&red, b"eXIf", b"MM\0*exifdata"));
    assert_eq!(plain.scope, HashScope::MediaPayload1);
    assert_eq!(plain.fallback_reason, None);
    assert_eq!(text.scope, HashScope::MediaPayload1);
    assert_eq!(exif.scope, HashScope::MediaPayload1);
    assert_eq!(plain.hash, text.hash);
    assert_eq!(plain.hash, exif.hash);
}

#[test]
fn png_hash_differs_for_different_pixels() {
    let r = hash_named("x.png", &read("red.png"));
    let b = hash_named("x.png", &read("blue.png"));
    assert_ne!(r.hash, b.hash);
}

#[test]
fn webp_hash_ignores_exif_and_vp8x() {
    let red = read("red.webp");
    let plain = hash_named("x.webp", &red);
    let exif = hash_named("x.webp", &webp_add_exif(&red, b"Exif\0\0odd"));
    assert_eq!(plain.scope, HashScope::MediaPayload1);
    assert_eq!(plain.fallback_reason, None);
    assert_eq!(exif.scope, HashScope::MediaPayload1);
    assert_eq!(plain.hash, exif.hash);
}

#[test]
fn webp_hash_differs_for_different_pixels() {
    let r = hash_named("x.webp", &read("red.webp"));
    let b = hash_named("x.webp", &read("blue.webp"));
    assert_ne!(r.hash, b.hash);
}

#[test]
fn truncated_png_and_webp_fall_back_without_panic() {
    for name in ["red.png", "red.webp"] {
        let bytes = read(name);
        let out = hash_named(name, &bytes[..bytes.len() / 2]);
        assert_eq!(out.scope, HashScope::File1, "{name}");
        assert!(out.fallback_reason.is_some(), "{name}");
    }
}

#[test]
fn red_png_payload_hash_is_pinned() {
    // Persisted in sidecars: changing this value breaks re-linking.
    let out = hash_named("x.png", &read("red.png"));
    assert_eq!(out.scope, HashScope::MediaPayload1);
    assert_eq!(
        out.hash.to_string(),
        "blake3:6e44a4f03af1007b904d747afad03992db2b588a2dcc294ab16b0ae7019c427b"
    );
}

#[test]
fn red_webp_payload_hash_is_pinned() {
    // Persisted in sidecars: changing this value breaks re-linking.
    let out = hash_named("x.webp", &read("red.webp"));
    assert_eq!(out.scope, HashScope::MediaPayload1);
    assert_eq!(
        out.hash.to_string(),
        "blake3:9673942147bea392bcc60d165643d457ec0b2e64626980dbb4b2480134c45c02"
    );
}
