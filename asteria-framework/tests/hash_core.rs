use asteria_framework::Error;
use asteria_framework::hash::{
    ContentHash, HashScope, MediaFormat, content_hash, detect_format, detect_format_file,
};

#[test]
fn content_hash_display_roundtrip() {
    let s = format!("blake3:{}", "ab01".repeat(16));
    let h: ContentHash = s.parse().unwrap();
    assert_eq!(h.to_string(), s);
    let sha = format!("sha256:{}", "ab01".repeat(16));
    assert!(matches!(
        sha.parse::<ContentHash>(),
        Err(Error::InvalidHash(_))
    ));
    let short = format!("blake3:{}", "a".repeat(63));
    assert!(short.parse::<ContentHash>().is_err());
    let upper = format!("blake3:{}", "AB01".repeat(16));
    assert!(upper.parse::<ContentHash>().is_err());
}

#[test]
fn content_hash_serde_is_string() {
    let s = format!("blake3:{}", "ab01".repeat(16));
    let h: ContentHash = s.parse().unwrap();
    let json = serde_json::to_string(&h).unwrap();
    assert_eq!(json, format!("\"{s}\""));
    assert_eq!(serde_json::from_str::<ContentHash>(&json).unwrap(), h);
}

#[test]
fn scope_roundtrip() {
    for s in ["media-payload/1", "file/1"] {
        assert_eq!(s.parse::<HashScope>().unwrap().to_string(), s);
    }
    assert!(matches!(
        "media-payload/2".parse::<HashScope>(),
        Err(Error::InvalidHashScope(_))
    ));
}

#[test]
fn detects_by_magic_bytes() {
    assert_eq!(detect_format(&[0xFF, 0xD8, 0xFF, 0xE0]), MediaFormat::Jpeg);
    assert_eq!(
        detect_format(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        MediaFormat::Png
    );
    assert_eq!(
        detect_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
        MediaFormat::WebP
    );
    assert_eq!(
        detect_format(b"RIFF\x00\x00\x00\x00WAVEfmt "),
        MediaFormat::Unknown
    );
    assert_eq!(detect_format(b"fLaC\x00\x00\x00\x22"), MediaFormat::Flac);
    assert_eq!(
        detect_format(b"ID3\x04\x00\x00\x00\x00\x00\x00"),
        MediaFormat::Mp3
    );
    assert_eq!(detect_format(&[0xFF, 0xFB, 0x90, 0x00]), MediaFormat::Mp3);
    assert_eq!(detect_format(&[0xFF, 0xF3, 0x90, 0x00]), MediaFormat::Mp3);
    assert_eq!(detect_format(&[0xFF, 0xC0]), MediaFormat::Unknown);
    assert_eq!(detect_format(b"hello"), MediaFormat::Unknown);
    assert_eq!(detect_format(b""), MediaFormat::Unknown);
}

#[test]
fn detects_by_magic_not_extension() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.jpg");
    std::fs::write(&p, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]).unwrap();
    assert_eq!(detect_format_file(&p).unwrap(), MediaFormat::Png);
}

#[test]
fn detects_flac_behind_id3v2() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.mp3");
    // ID3v2.4, flags 0, syncsafe size 5, then 5 tag bytes, then fLaC
    let mut data = b"ID3\x04\x00\x00\x00\x00\x00\x05".to_vec();
    data.extend_from_slice(&[0; 5]);
    data.extend_from_slice(b"fLaC\x00\x00\x00\x22");
    std::fs::write(&p, &data).unwrap();
    assert_eq!(detect_format_file(&p).unwrap(), MediaFormat::Flac);

    // footer flag adds 10 bytes
    let mut data = b"ID3\x04\x00\x10\x00\x00\x00\x05".to_vec();
    data.extend_from_slice(&[0; 15]);
    data.extend_from_slice(b"fLaC\x00\x00\x00\x22");
    std::fs::write(&p, &data).unwrap();
    assert_eq!(detect_format_file(&p).unwrap(), MediaFormat::Flac);

    // v2.3 with the 0x10 bit set has no footer
    let mut data = b"ID3\x03\x00\x10\x00\x00\x00\x05".to_vec();
    data.extend_from_slice(&[0; 5]);
    data.extend_from_slice(b"fLaC\x00\x00\x00\x22");
    std::fs::write(&p, &data).unwrap();
    assert_eq!(detect_format_file(&p).unwrap(), MediaFormat::Flac);

    // ID3 followed by something else stays Mp3
    let mut data = b"ID3\x03\x00\x00\x00\x00\x00\x05".to_vec();
    data.extend_from_slice(&[0; 5]);
    data.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    std::fs::write(&p, &data).unwrap();
    assert_eq!(detect_format_file(&p).unwrap(), MediaFormat::Mp3);

    // truncated: tag claims more than the file holds
    std::fs::write(&p, b"ID3\x04\x00\x00\x00\x00\x7f\x7f").unwrap();
    assert_eq!(detect_format_file(&p).unwrap(), MediaFormat::Mp3);
}

#[test]
fn unknown_format_falls_back_to_file_hash() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.txt");
    std::fs::write(&p, b"hello").unwrap();
    let out = content_hash(&p).unwrap();
    assert_eq!(out.scope, HashScope::File1);
    assert_eq!(out.fallback_reason, None);
    assert_eq!(
        out.hash.to_string(),
        format!("blake3:{}", blake3::hash(b"hello").to_hex())
    );
}

#[test]
fn missing_file_is_io_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        content_hash(&dir.path().join("nope")),
        Err(Error::Io(_))
    ));
}
