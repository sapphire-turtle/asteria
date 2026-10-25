//! Exact-bytes golden files for the XMP writer.

use std::path::PathBuf;

use asteria_framework::hash::{ContentHash, HashScope};
use asteria_framework::{RatingEntry, Sidecar, UserId};
use chrono::{TimeZone, Utc};

fn xmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/xmp")
        .join(name)
}

fn assert_golden(name: &str, actual: &[u8]) {
    let path = xmp(&format!("golden/{name}"));
    let expected = std::fs::read(&path).unwrap();
    assert!(
        expected == actual,
        "{name} differs\n--- expected\n{}\n--- actual\n{}",
        String::from_utf8_lossy(&expected),
        String::from_utf8_lossy(actual)
    );
}

#[test]
fn new_full_sidecar_matches_golden() {
    let mut sc = Sidecar::new();
    sc.rating = Some(RatingEntry {
        value: 4,
        modified: Some(Utc.with_ymd_and_hms(2026, 10, 22, 9, 30, 0).unwrap()),
    });
    let alice: UserId = "123abcd".parse().unwrap();
    let bob: UserId = "456efgh".parse().unwrap();
    sc.user_ratings.insert(
        alice,
        RatingEntry {
            value: 5,
            modified: Some(Utc.with_ymd_and_hms(2026, 10, 21, 20, 0, 0).unwrap()),
        },
    );
    sc.user_ratings.insert(
        bob,
        RatingEntry {
            value: -1,
            modified: Some(
                Utc.with_ymd_and_hms(2026, 10, 21, 20, 0, 1).unwrap()
                    + chrono::Duration::milliseconds(250),
            ),
        },
    );
    sc.subjects
        .extend(["cat".to_string(), "京都 & <b>".to_string()]);
    sc.hierarchical_subjects
        .extend(["animal|cat".to_string(), "place|京都".to_string()]);
    sc.description = Some("A \"cat\" & more".into());
    sc.label = Some("Red".into());
    let hash: ContentHash = format!("blake3:{}", "0123456789abcdef".repeat(4))
        .parse()
        .unwrap();
    sc.content_hash = Some((hash, HashScope::MediaPayload1));
    assert_golden("new_full.xmp", &sc.to_bytes());
}

#[test]
fn darktable_rating_change_matches_golden() {
    let mut sc = Sidecar::read(&xmp("darktable_attr.xmp")).unwrap().unwrap();
    sc.rating.as_mut().unwrap().value = 5;
    assert_golden("darktable_attr_rating5.xmp", &sc.to_bytes());
}
