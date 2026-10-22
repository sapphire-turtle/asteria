mod common;

use std::fs;
use std::path::Path;

use asteria_framework::hash::{HashScope, content_hash};
use asteria_framework::journal::{Actor, Journal, OpKind};
use asteria_framework::{Error, Library, UserId};
use chrono::{TimeZone, Utc};
use grain_id::GrainId;

fn setup(rel: &str) -> (tempfile::TempDir, Library) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::init(dir.path(), "fluo10").unwrap();
    let dest = dir.path().join(rel);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::copy(common::fixture("red.jpg"), dest).unwrap();
    (dir, lib)
}

fn t(s: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, s).unwrap()
}

#[test]
fn no_sidecar_is_created_for_noop_update() {
    let (dir, lib) = setup("red.jpg");
    let wrote = lib
        .update_sidecar(Path::new("red.jpg"), Actor::User(lib.owner()), |_| {})
        .unwrap();
    assert!(!wrote);
    assert!(!dir.path().join("red.jpg.xmp").exists());
    assert!(Journal::new(&lib).read_all().unwrap().is_empty());
}

#[test]
fn first_rating_creates_sidecar_with_payload_hash() {
    let (dir, lib) = setup("red.jpg");
    let owner = lib.owner();
    lib.set_rating(Path::new("red.jpg"), owner, 4, t(1), Actor::User(owner))
        .unwrap();
    assert!(dir.path().join("red.jpg.xmp").exists());
    let sc = lib.read_sidecar(Path::new("red.jpg")).unwrap().unwrap();
    assert_eq!(sc.rating.unwrap().value, 4);
    let expected = content_hash(&dir.path().join("red.jpg")).unwrap();
    let (hash, scope) = sc.content_hash.unwrap();
    assert_eq!(hash, expected.hash);
    assert_eq!(scope, HashScope::MediaPayload1);
    let entries = Journal::new(&lib).read_all().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, OpKind::WriteSidecar);
    assert_eq!(entries[0].after["path"], "red.jpg");
    assert_eq!(entries[0].after["fields"][0], "rating");
}

#[test]
fn unchanged_update_on_existing_sidecar_writes_nothing() {
    let (_dir, lib) = setup("red.jpg");
    let owner = lib.owner();
    let a = Actor::User(owner);
    lib.set_rating(Path::new("red.jpg"), owner, 4, t(1), a)
        .unwrap();
    assert!(!lib.update_sidecar(Path::new("red.jpg"), a, |_| {}).unwrap());
    assert_eq!(Journal::new(&lib).read_all().unwrap().len(), 1);
}

#[test]
fn non_owner_rating_goes_to_user_ratings() {
    let (_dir, lib) = setup("red.jpg");
    let other = UserId(GrainId::random());
    lib.set_rating(Path::new("red.jpg"), other, 5, t(1), Actor::User(other))
        .unwrap();
    let sc = lib.read_sidecar(Path::new("red.jpg")).unwrap().unwrap();
    assert!(sc.rating.is_none());
    assert_eq!(sc.user_ratings.len(), 1);
    assert_eq!(sc.user_ratings[&other].value, 5);
}

#[test]
fn older_rating_does_not_overwrite_newer() {
    let (_dir, lib) = setup("red.jpg");
    let owner = lib.owner();
    let a = Actor::User(owner);
    let p = Path::new("red.jpg");
    lib.set_rating(p, owner, 4, t(2), a).unwrap();
    lib.set_rating(p, owner, 2, t(1), a).unwrap();
    assert_eq!(
        lib.read_sidecar(p).unwrap().unwrap().rating.unwrap().value,
        4
    );
    // Equal timestamp overwrites.
    lib.set_rating(p, owner, 3, t(2), a).unwrap();
    assert_eq!(
        lib.read_sidecar(p).unwrap().unwrap().rating.unwrap().value,
        3
    );
}

#[test]
fn malformed_sidecar_is_not_overwritten() {
    let (dir, lib) = setup("red.jpg");
    let sc = dir.path().join("red.jpg.xmp");
    fs::copy(common::fixture("xmp/malformed.xmp"), &sc).unwrap();
    let before = fs::read(&sc).unwrap();
    let owner = lib.owner();
    let err = lib
        .set_rating(Path::new("red.jpg"), owner, 4, t(1), Actor::User(owner))
        .unwrap_err();
    assert!(matches!(err, Error::MalformedSidecar { .. }));
    assert_eq!(fs::read(&sc).unwrap(), before);
    assert!(Journal::new(&lib).read_all().unwrap().is_empty());
}

#[test]
fn japanese_media_path_works() {
    let (dir, lib) = setup("京都旅行/red.jpg");
    let owner = lib.owner();
    lib.set_rating(
        Path::new("京都旅行/red.jpg"),
        owner,
        3,
        t(1),
        Actor::User(owner),
    )
    .unwrap();
    assert!(dir.path().join("京都旅行/red.jpg.xmp").exists());
    let e = &Journal::new(&lib).read_all().unwrap()[0];
    assert_eq!(e.after["path"], "京都旅行/red.jpg");
}

#[test]
fn invalid_rating_rejected() {
    let (_dir, lib) = setup("red.jpg");
    let owner = lib.owner();
    for v in [6, -2] {
        let err = lib
            .set_rating(Path::new("red.jpg"), owner, v, t(1), Actor::User(owner))
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRating(x) if x == v));
    }
}

#[test]
fn missing_media_is_reported() {
    let (_dir, lib) = setup("red.jpg");
    let err = lib
        .update_sidecar(Path::new("nope.jpg"), Actor::User(lib.owner()), |_| {})
        .unwrap_err();
    assert!(matches!(err, Error::MediaNotFound(_)));
}

#[test]
fn path_escape_is_rejected() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Library::init(&root, "fluo10").unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::copy(common::fixture("red.jpg"), root.join("sub/red.jpg")).unwrap();
    fs::copy(common::fixture("red.jpg"), outer.path().join("outside.jpg")).unwrap();
    let a = Actor::User(lib.owner());
    let abs = root.join("sub/red.jpg");
    for p in [
        Path::new("../outside.jpg"),
        Path::new("sub/../../outside.jpg"),
        abs.as_path(),
    ] {
        let err = lib
            .update_sidecar(p, a, |s| s.label = Some("x".into()))
            .unwrap_err();
        assert!(matches!(err, Error::MediaNotFound(_)), "{p:?}");
        assert!(matches!(lib.read_sidecar(p), Err(Error::MediaNotFound(_))));
    }
    assert!(!outer.path().join("outside.jpg.xmp").exists());
    assert!(!root.join("sub/red.jpg.xmp").exists());
}

#[test]
fn meta_dir_and_sidecar_paths_are_rejected() {
    let (dir, lib) = setup("red.jpg");
    let a = Actor::User(lib.owner());
    for p in [".asteria/library.toml", "red.jpg.xmp"] {
        let err = lib
            .update_sidecar(Path::new(p), a, |s| s.label = Some("x".into()))
            .unwrap_err();
        assert!(matches!(err, Error::MediaNotFound(_)), "{p}");
    }
    assert!(!dir.path().join("red.jpg.xmp").exists());
    assert!(!dir.path().join(".asteria/library.toml.xmp").exists());
    assert!(!dir.path().join("red.jpg.xmp.xmp").exists());
}

#[test]
fn read_sidecar_reports_malformed() {
    let (dir, lib) = setup("red.jpg");
    fs::copy(
        common::fixture("xmp/malformed.xmp"),
        dir.path().join("red.jpg.xmp"),
    )
    .unwrap();
    let err = lib.read_sidecar(Path::new("red.jpg")).unwrap_err();
    assert!(matches!(err, Error::MalformedSidecar { .. }));
}

#[test]
fn older_non_owner_rating_does_not_overwrite_newer() {
    let (_dir, lib) = setup("red.jpg");
    let other = UserId(GrainId::random());
    let a = Actor::User(other);
    let p = Path::new("red.jpg");
    lib.set_rating(p, other, 4, t(2), a).unwrap();
    lib.set_rating(p, other, 2, t(1), a).unwrap();
    let sc = lib.read_sidecar(p).unwrap().unwrap();
    assert_eq!(sc.user_ratings[&other].value, 4);
    lib.set_rating(p, other, 3, t(2), a).unwrap();
    let sc = lib.read_sidecar(p).unwrap().unwrap();
    assert_eq!(sc.user_ratings[&other].value, 3);
}
