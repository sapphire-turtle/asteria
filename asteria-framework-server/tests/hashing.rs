use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use asteria_framework::journal::{Actor, Journal, OpKind};
use asteria_framework::{Library, MediaKind};
use asteria_framework_server::{Cache, MediaFilter, hash_pending, relink_orphans, scan};
use chrono::Utc;

struct Gallery;
impl MediaFilter for Gallery {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../asteria-framework/tests/fixtures")
        .join(name)
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    lib: Library,
    cache: Cache,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Library::init(&root, "owner").unwrap();
    let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
    Env {
        _dir: dir,
        root,
        lib,
        cache,
    }
}

fn red(to: &Path) {
    fs::copy(fixture("red.jpg"), to).unwrap();
}

fn rate(e: &Env, rel: &str, v: i8) {
    let owner = e.lib.owner();
    e.lib
        .set_rating(Path::new(rel), owner, v, Utc::now(), Actor::User(owner))
        .unwrap();
}

fn scan_all(e: &Env) {
    scan(&e.lib, &e.cache, &Gallery).unwrap();
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn hashes_missing_and_stale() {
    let e = env();
    red(&e.root.join("a.jpg"));
    fs::copy(fixture("red.jpg"), e.root.join("b.jpg")).unwrap();
    scan_all(&e);
    assert_eq!(hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap(), 2);
    assert_eq!(hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap(), 0);
    let h = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash.unwrap();
    assert!(h.hash.starts_with("blake3:"));
    assert_eq!(h.scope, "media-payload/1");

    // Drop one hash by touching the file: mtime differs, so only that one is recomputed.
    let f = fs::OpenOptions::new()
        .write(true)
        .open(e.root.join("a.jpg"))
        .unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(100))
        .unwrap();
    drop(f);
    assert_eq!(hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap(), 1);
    let h2 = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash.unwrap();
    assert_ne!(h2.mtime_ns, h.mtime_ns);
    assert_eq!(h2.hash, h.hash);
}

#[test]
fn cancel_stops_early() {
    let e = env();
    red(&e.root.join("a.jpg"));
    scan_all(&e);
    let cancel = AtomicBool::new(true);
    assert_eq!(hash_pending(&e.lib, &e.cache, &cancel).unwrap(), 0);
    assert!(
        e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .hash
            .is_none()
    );
}

#[test]
fn vanished_file_is_skipped() {
    let e = env();
    red(&e.root.join("a.jpg"));
    scan_all(&e);
    fs::remove_file(e.root.join("a.jpg")).unwrap();
    assert_eq!(hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap(), 0);
}

#[test]
fn relinks_unique_match() {
    let e = env();
    red(&e.root.join("red.jpg"));
    rate(&e, "red.jpg", 4);
    fs::create_dir(e.root.join("sub")).unwrap();
    fs::rename(e.root.join("red.jpg"), e.root.join("sub/renamed.jpg")).unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();

    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert_eq!(
        r.relinked,
        vec![("red.jpg.xmp".to_string(), "sub/renamed.jpg".to_string())]
    );
    assert!(r.ambiguous.is_empty() && r.unmatched.is_empty());
    assert!(!e.root.join("red.jpg.xmp").exists());
    assert!(e.root.join("sub/renamed.jpg.xmp").exists());
    let sc = e
        .lib
        .read_sidecar(Path::new("sub/renamed.jpg"))
        .unwrap()
        .unwrap();
    assert_eq!(sc.rating.unwrap().value, 4);
    let rec = e.cache.get_by_path("sub/renamed.jpg").unwrap().unwrap();
    assert!(rec.sidecar.has_sidecar);
    assert_eq!(rec.sidecar.owner_rating, Some(4));

    let entries: Vec<_> = Journal::new(&e.lib)
        .read_all()
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == OpKind::RelinkSidecar)
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].before["path"], "red.jpg.xmp");
    assert_eq!(entries[0].after["path"], "sub/renamed.jpg.xmp");
    assert_eq!(entries[0].actor, Actor::User(e.lib.owner()));
    assert!(entries[0].plan_id.is_none());
}

#[test]
fn ambiguous_match_is_not_moved() {
    let e = env();
    red(&e.root.join("red.jpg"));
    rate(&e, "red.jpg", 3);
    fs::rename(e.root.join("red.jpg"), e.root.join("one.jpg")).unwrap();
    red(&e.root.join("two.jpg"));
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty());
    assert_eq!(r.ambiguous, vec!["red.jpg.xmp".to_string()]);
    assert!(e.root.join("red.jpg.xmp").exists());
}

#[test]
fn unmatched_and_malformed_are_not_moved() {
    let e = env();
    red(&e.root.join("red.jpg"));
    rate(&e, "red.jpg", 2);
    fs::remove_file(e.root.join("red.jpg")).unwrap();
    fs::write(e.root.join("broken.jpg.xmp"), b"<not xml").unwrap();
    // Content that matches no sidecar.
    fs::write(e.root.join("other.jpg"), b"not an image at all").unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty() && r.ambiguous.is_empty());
    let mut u = r.unmatched.clone();
    u.sort();
    assert_eq!(u, vec!["broken.jpg.xmp", "red.jpg.xmp"]);
    assert!(e.root.join("broken.jpg.xmp").exists());
    assert!(e.root.join("red.jpg.xmp").exists());
}

#[test]
fn target_with_existing_sidecar_is_not_overwritten() {
    let e = env();
    red(&e.root.join("red.jpg"));
    rate(&e, "red.jpg", 4);
    fs::rename(e.root.join("red.jpg"), e.root.join("new.jpg")).unwrap();
    // The new location already has its own sidecar (written outside the cache's knowledge).
    fs::write(e.root.join("new.jpg.xmp"), b"existing").unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty());
    assert_eq!(fs::read(e.root.join("new.jpg.xmp")).unwrap(), b"existing");
    assert!(e.root.join("red.jpg.xmp").exists());
}

#[test]
fn hidden_and_meta_dirs_are_ignored() {
    let e = env();
    fs::create_dir(e.root.join(".hidden")).unwrap();
    fs::write(e.root.join(".hidden/x.jpg.xmp"), b"x").unwrap();
    fs::write(e.root.join(".asteria/y.jpg.xmp"), b"x").unwrap();
    scan_all(&e);
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty() && r.ambiguous.is_empty() && r.unmatched.is_empty());
}
