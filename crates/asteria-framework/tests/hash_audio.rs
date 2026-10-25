mod common;

use std::path::Path;

use asteria_framework::hash::{HashOutcome, HashScope, content_hash};
use common::fixture;
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{Tag, TagType};

const FIXTURES: [&str; 4] = ["sine440.flac", "sine880.flac", "sine440.mp3", "sine880.mp3"];

/// Copies a fixture into `dir` and writes `tag_type` tags (title and, when
/// `picture`, an embedded cover) into the copy with lofty.
fn tagged(dir: &Path, name: &str, tag_type: TagType, picture: bool) -> std::path::PathBuf {
    let path = dir.join(format!("{tag_type:?}-{name}"));
    std::fs::copy(fixture(name), &path).unwrap();
    let mut file = lofty::read_from_path(&path).unwrap();
    let mut tag = Tag::new(tag_type);
    tag.set_title("A title".to_string());
    tag.set_artist("An artist".to_string());
    if picture {
        tag.push_picture(Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Png),
            None,
            vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4, 5, 6, 7, 8],
        ));
    }
    file.insert_tag(tag);
    file.save_to_path(&path, WriteOptions::default()).unwrap();
    path
}

fn hash_fixture(name: &str) -> HashOutcome {
    content_hash(&fixture(name)).unwrap()
}

#[test]
fn flac_hash_ignores_vorbis_comments_and_pictures() {
    let dir = tempfile::tempdir().unwrap();
    let plain = hash_fixture("sine440.flac");
    for (tag_type, picture) in [
        (TagType::VorbisComments, false),
        (TagType::VorbisComments, true),
    ] {
        let p = tagged(dir.path(), "sine440.flac", tag_type, picture);
        assert_ne!(
            std::fs::read(&p).unwrap(),
            std::fs::read(fixture("sine440.flac")).unwrap(),
            "tagging must change the file"
        );
        let out = content_hash(&p).unwrap();
        assert_eq!(out.scope, HashScope::MediaPayload1);
        assert_eq!(out.fallback_reason, None);
        assert_eq!(out.hash, plain.hash);
    }
}

#[test]
fn flac_hash_ignores_leading_id3v2() {
    let mut bytes = b"ID3\x04\x00\x00\x00\x00\x00\x08tagbytes".to_vec();
    bytes.extend(std::fs::read(fixture("sine440.flac")).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("id3.flac");
    std::fs::write(&p, &bytes).unwrap();
    let out = content_hash(&p).unwrap();
    assert_eq!(out.scope, HashScope::MediaPayload1);
    assert_eq!(out.hash, hash_fixture("sine440.flac").hash);
}

#[test]
fn flac_hash_ignores_appended_id3v1() {
    let mut bytes = std::fs::read(fixture("sine440.flac")).unwrap();
    bytes.extend_from_slice(b"TAG");
    bytes.resize(bytes.len() + 125, b'x');
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("v1.flac");
    std::fs::write(&p, &bytes).unwrap();
    let out = content_hash(&p).unwrap();
    assert_eq!(out.scope, HashScope::MediaPayload1);
    assert_eq!(out.hash, hash_fixture("sine440.flac").hash);
}

#[test]
fn flac_hash_differs_for_different_audio() {
    assert_ne!(
        hash_fixture("sine440.flac").hash,
        hash_fixture("sine880.flac").hash
    );
}

#[test]
fn mp3_hash_ignores_id3v2_id3v1_and_ape() {
    let dir = tempfile::tempdir().unwrap();
    let plain = hash_fixture("sine440.mp3");
    for (tag_type, picture) in [
        (TagType::Id3v2, false),
        (TagType::Id3v2, true),
        (TagType::Id3v1, false),
        (TagType::Ape, false),
    ] {
        let p = tagged(dir.path(), "sine440.mp3", tag_type, picture);
        assert_ne!(
            std::fs::read(&p).unwrap(),
            std::fs::read(fixture("sine440.mp3")).unwrap(),
            "{tag_type:?} must change the file"
        );
        let out = content_hash(&p).unwrap();
        assert_eq!(out.scope, HashScope::MediaPayload1, "{tag_type:?}");
        assert_eq!(out.fallback_reason, None);
        assert_eq!(out.hash, plain.hash, "{tag_type:?}");
    }
}

#[test]
fn mp3_hash_ignores_all_tags_together() {
    let dir = tempfile::tempdir().unwrap();
    let p = tagged(dir.path(), "sine440.mp3", TagType::Id3v2, true);
    for tag_type in [TagType::Id3v1, TagType::Ape] {
        let mut file = lofty::read_from_path(&p).unwrap();
        let mut tag = Tag::new(tag_type);
        tag.set_title("T".to_string());
        file.insert_tag(tag);
        file.save_to_path(&p, WriteOptions::default()).unwrap();
    }
    let out = content_hash(&p).unwrap();
    assert_eq!(out.scope, HashScope::MediaPayload1);
    assert_eq!(out.hash, hash_fixture("sine440.mp3").hash);
}

#[test]
fn mp3_hash_differs_for_different_audio() {
    assert_ne!(
        hash_fixture("sine440.mp3").hash,
        hash_fixture("sine880.mp3").hash
    );
}

#[test]
fn audio_hash_scope_is_media_payload() {
    for name in FIXTURES {
        let out = hash_fixture(name);
        assert_eq!(out.scope, HashScope::MediaPayload1, "{name}");
        assert_eq!(out.fallback_reason, None, "{name}");
    }
}

#[test]
fn truncated_flac_header_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("short.flac");
    let bytes = std::fs::read(fixture("sine440.flac")).unwrap();
    std::fs::write(&p, &bytes[..20]).unwrap();
    let out = content_hash(&p).unwrap();
    assert_eq!(out.scope, HashScope::File1);
    assert!(out.fallback_reason.is_some());
}

#[test]
fn tag_only_mp3_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("tags.mp3");
    let mut bytes = b"ID3\x03\x00\x00\x00\x00\x00\x02ab".to_vec();
    bytes.extend_from_slice(b"TAG");
    bytes.resize(bytes.len() + 125, 0);
    std::fs::write(&p, &bytes).unwrap();
    let out = content_hash(&p).unwrap();
    assert_eq!(out.scope, HashScope::File1);
    assert!(out.fallback_reason.is_some());
}

#[test]
fn fixture_hashes_equal_blake3_of_known_audio_ranges() {
    // sine440.flac: fLaC + STREAMINFO + VORBIS_COMMENT + an 8192-byte PADDING
    // (last) block end at byte 8256 (checked independently by walking the block
    // headers); sine440.mp3 has no tags.
    let flac = std::fs::read(fixture("sine440.flac")).unwrap();
    let expect = |b: &[u8]| format!("blake3:{}", blake3::hash(b).to_hex());
    assert_eq!(
        hash_fixture("sine440.flac").hash.to_string(),
        expect(&flac[8256..])
    );
    let mp3 = std::fs::read(fixture("sine440.mp3")).unwrap();
    assert_eq!(hash_fixture("sine440.mp3").hash.to_string(), expect(&mp3));
}

#[test]
fn audio_payload_hashes_are_pinned() {
    // Persisted in sidecars: changing these values breaks re-linking.
    assert_eq!(
        hash_fixture("sine440.flac").hash.to_string(),
        "blake3:cd1d6511bcb9e258b08dd3654cc4cee8cca46fa787e027141df37fb1e8855137"
    );
    assert_eq!(
        hash_fixture("sine440.mp3").hash.to_string(),
        "blake3:0768c1ae157468684bf0885a7b7bbcbb8fa9dbbd920ef52e75e252f9ee10b681"
    );
}
