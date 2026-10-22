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

#[test]
fn mp3_detection_requires_a_valid_frame_header() {
    // MPEG-1 Layer III, 128 kbit/s, 44.1 kHz.
    assert_eq!(detect_format(&[0xFF, 0xFB, 0x90, 0x64]), MediaFormat::Mp3);
    // UTF-16LE BOM followed by text ("<?xml"): sample-rate index 11 is reserved.
    assert_eq!(
        detect_format(b"\xFF\xFE<\x00?\x00x\x00m\x00l\x00"),
        MediaFormat::Unknown
    );
    // Reserved version (01).
    assert_eq!(
        detect_format(&[0xFF, 0xEB, 0x90, 0x00]),
        MediaFormat::Unknown
    );
    // Reserved layer (00); this also leaves AAC ADTS (FF F1) unmatched.
    assert_eq!(
        detect_format(&[0xFF, 0xF9, 0x90, 0x00]),
        MediaFormat::Unknown
    );
    assert_eq!(
        detect_format(&[0xFF, 0xF1, 0x50, 0x80]),
        MediaFormat::Unknown
    );
    // Bitrate index 1111.
    assert_eq!(
        detect_format(&[0xFF, 0xFB, 0xF0, 0x00]),
        MediaFormat::Unknown
    );
    // Sync word alone, header cut short.
    assert_eq!(detect_format(&[0xFF, 0xFB]), MediaFormat::Unknown);
}

#[test]
fn sine_fixtures_still_detect_as_mp3() {
    for name in ["sine440.mp3", "sine880.mp3"] {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        assert_eq!(detect_format_file(&p).unwrap(), MediaFormat::Mp3, "{name}");
    }
}

#[test]
fn content_hash_bytes_and_order() {
    let mut a = [0u8; 32];
    a[31] = 1;
    let mut b = [0u8; 32];
    b[0] = 1;
    let (ha, hb) = (ContentHash::from_bytes(a), ContentHash::from_bytes(b));
    assert_eq!(ha.as_bytes(), &a);
    assert!(ha < hb);
    assert_eq!(ha.cmp(&ha), std::cmp::Ordering::Equal);
    let set: std::collections::BTreeSet<_> = [hb, ha].into();
    assert_eq!(set.into_iter().collect::<Vec<_>>(), vec![ha, hb]);
}

#[test]
fn hash_scope_serde_is_string() {
    for (scope, s) in [
        (HashScope::MediaPayload1, "media-payload/1"),
        (HashScope::File1, "file/1"),
    ] {
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(json, format!("\"{s}\""));
        assert_eq!(serde_json::from_str::<HashScope>(&json).unwrap(), scope);
    }
    assert!(serde_json::from_str::<HashScope>("\"media-payload/2\"").is_err());
}
