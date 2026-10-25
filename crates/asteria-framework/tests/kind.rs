use asteria_framework::{MediaKind, classify};
use std::fs;

#[test]
fn classify_by_extension_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    for (name, kind) in [
        ("a.HEIC", MediaKind::Image),
        ("b.MOV", MediaKind::Video),
        ("c.opus", MediaKind::Audio),
        ("d.txt", MediaKind::Other),
        ("noext", MediaKind::Other),
    ] {
        let p = dir.path().join(name);
        fs::write(&p, b"not magic").unwrap();
        assert_eq!(classify(&p).unwrap(), kind, "{name}");
    }
}

#[test]
fn magic_overrides_extension() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.mov");
    fs::copy("tests/fixtures/blue.png", &p).unwrap();
    assert_eq!(classify(&p).unwrap(), MediaKind::Image);
    let q = dir.path().join("y.bin");
    fs::copy("tests/fixtures/sine440.mp3", &q).unwrap();
    assert_eq!(classify(&q).unwrap(), MediaKind::Audio);
}

#[test]
fn media_kind_serializes_snake_case() {
    assert_eq!(
        serde_json::to_string(&MediaKind::Video).unwrap(),
        "\"video\""
    );
}
