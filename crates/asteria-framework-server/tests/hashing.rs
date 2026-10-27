use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use asteria_framework::journal::{Actor, OpKind};
use asteria_framework::{Library, MediaKind};
use asteria_framework_server::{Cache, MediaFilter, hash_pending, relink_orphans, scan};
use chrono::Utc;

struct Photos;
impl MediaFilter for Photos {
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
    let lib = Library::init(&root, "owner")
        .unwrap()
        .with_state_dir(dir.path().join("state"));
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
    scan(&e.lib, &e.cache, &Photos).unwrap();
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
    // Without a rescan the record's mtime is stale, so the new hash is refused.
    assert_eq!(hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap(), 0);
    scan_all(&e);
    assert_eq!(hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap(), 1);
    let h2 = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash.unwrap();
    assert_ne!(h2.mtime_ns, h.mtime_ns);
    assert_eq!(h2.hash, h.hash);
}

#[test]
fn metadata_only_edit_keeps_payload_hash_but_changes_file_hash() {
    let e = env();
    red(&e.root.join("a.jpg"));
    fs::write(e.root.join("notes.bin"), b"not media").unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let h = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash.unwrap();
    let whole = asteria_framework::hash::file_hash(&e.root.join("a.jpg")).unwrap();
    assert_eq!(h.file_hash, whole.to_string());
    assert_ne!(h.file_hash, h.hash);

    // Insert a COM segment after SOI: metadata only, so the payload hash stays.
    let mut bytes = fs::read(e.root.join("a.jpg")).unwrap();
    bytes.splice(2..2, [0xFF, 0xFE, 0x00, 0x05, b'h', b'i', b'!']);
    fs::write(e.root.join("a.jpg"), &bytes).unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let h2 = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash.unwrap();
    assert_eq!(h2.hash, h.hash);
    assert_ne!(h2.file_hash, h.file_hash);
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

    let entries: Vec<_> = e
        .lib
        .journal()
        .unwrap()
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
    assert!(r.relinked.is_empty() && r.ambiguous.is_empty() && r.unmatched.is_empty());
    assert_eq!(r.conflicts, vec!["red.jpg.xmp".to_string()]);
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

#[test]
fn two_orphans_one_target_are_both_ambiguous() {
    let e = env();
    red(&e.root.join("a.jpg"));
    rate(&e, "a.jpg", 4);
    fs::copy(e.root.join("a.jpg.xmp"), e.root.join("b.jpg.xmp")).unwrap();
    fs::rename(e.root.join("a.jpg"), e.root.join("new.jpg")).unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty() && r.conflicts.is_empty());
    let mut a = r.ambiguous.clone();
    a.sort();
    assert_eq!(a, vec!["a.jpg.xmp", "b.jpg.xmp"]);
    assert!(e.root.join("a.jpg.xmp").exists() && e.root.join("b.jpg.xmp").exists());
    assert!(!e.root.join("new.jpg.xmp").exists());
}

#[test]
fn target_modified_after_hashing_is_not_moved() {
    let e = env();
    red(&e.root.join("red.jpg"));
    rate(&e, "red.jpg", 4);
    fs::rename(e.root.join("red.jpg"), e.root.join("new.jpg")).unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let f = fs::OpenOptions::new()
        .write(true)
        .open(e.root.join("new.jpg"))
        .unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(100))
        .unwrap();
    drop(f);
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty());
    assert_eq!(r.conflicts, vec!["red.jpg.xmp".to_string()]);
    assert!(e.root.join("red.jpg.xmp").exists());
    assert!(!e.root.join("new.jpg.xmp").exists());
}

#[cfg(unix)]
#[test]
fn interrupted_relink_is_completed() {
    let e = env();
    red(&e.root.join("red.jpg"));
    rate(&e, "red.jpg", 4);
    fs::rename(e.root.join("red.jpg"), e.root.join("new.jpg")).unwrap();
    // Simulate a crash after the link but before removing the old name.
    fs::hard_link(e.root.join("red.jpg.xmp"), e.root.join("new.jpg.xmp")).unwrap();
    scan_all(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert_eq!(
        r.relinked,
        vec![("red.jpg.xmp".to_string(), "new.jpg".to_string())]
    );
    assert!(!e.root.join("red.jpg.xmp").exists());
    assert!(e.root.join("new.jpg.xmp").exists());
    let n = e
        .lib
        .journal()
        .unwrap()
        .read_all()
        .unwrap()
        .iter()
        .filter(|j| j.kind == OpKind::RelinkSidecar)
        .count();
    assert_eq!(n, 1);
}

struct Music;
impl MediaFilter for Music {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Audio)
    }
}

fn scan_music(e: &Env) {
    scan(&e.lib, &e.cache, &Music).unwrap();
}

fn flac(to: &Path) {
    fs::copy(fixture("sine440.flac"), to).unwrap();
}

/// A copy of the FLAC fixture with a different title tag: same payload, different bytes.
fn flac_titled(to: &Path, title: &str) {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::prelude::*;
    use lofty::probe::Probe;
    use lofty::tag::{Tag, TagType};
    flac(to);
    let mut file = Probe::open(to).unwrap().read().unwrap();
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.set_title(title.to_string());
    file.insert_tag(tag);
    file.save_to_path(to, WriteOptions::default()).unwrap();
}

fn flac_env() -> Env {
    let e = env();
    for d in ["a", "b", "c"] {
        fs::create_dir(e.root.join(d)).unwrap();
    }
    e
}

#[test]
fn duplicate_with_sidecar_is_not_a_candidate() {
    let e = flac_env();
    flac(&e.root.join("a/x.flac"));
    flac(&e.root.join("b/x.flac"));
    rate(&e, "a/x.flac", 2);
    rate(&e, "b/x.flac", 5);
    fs::rename(e.root.join("a/x.flac"), e.root.join("c/x.flac")).unwrap();
    scan_music(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();

    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert_eq!(
        r.relinked,
        vec![("a/x.flac.xmp".to_string(), "c/x.flac".to_string())]
    );
    assert!(r.ambiguous.is_empty() && r.unmatched.is_empty() && r.conflicts.is_empty());
    let sc = e.lib.read_sidecar(Path::new("b/x.flac")).unwrap().unwrap();
    assert_eq!(sc.rating.unwrap().value, 5);
}

#[test]
fn file_hash_breaks_the_tie() {
    let e = flac_env();
    flac_titled(&e.root.join("a/x.flac"), "first");
    flac_titled(&e.root.join("b/x.flac"), "second");
    rate(&e, "b/x.flac", 4);
    fs::rename(e.root.join("b/x.flac"), e.root.join("c/x.flac")).unwrap();
    scan_music(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();

    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert_eq!(
        r.relinked,
        vec![("b/x.flac.xmp".to_string(), "c/x.flac".to_string())]
    );
    assert!(r.ambiguous.is_empty());
}

#[test]
fn identical_copies_stay_ambiguous() {
    let e = flac_env();
    flac(&e.root.join("a/x.flac"));
    rate(&e, "a/x.flac", 3);
    fs::rename(e.root.join("a/x.flac"), e.root.join("b/x.flac")).unwrap();
    flac(&e.root.join("c/x.flac"));
    scan_music(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty());
    assert_eq!(r.ambiguous, vec!["a/x.flac.xmp".to_string()]);
    assert!(e.root.join("a/x.flac.xmp").exists());
}

#[test]
fn no_file_hash_falls_back_to_ambiguous() {
    let e = flac_env();
    flac_titled(&e.root.join("a/x.flac"), "first");
    rate(&e, "a/x.flac", 3);
    let sidecar = e.root.join("a/x.flac.xmp");
    let mut sc = asteria_framework::Sidecar::read(&sidecar).unwrap().unwrap();
    assert!(sc.file_hash.is_some());
    sc.file_hash = None; // a sidecar written before FileHash existed
    sc.write(&sidecar).unwrap();
    fs::rename(e.root.join("a/x.flac"), e.root.join("b/x.flac")).unwrap();
    flac_titled(&e.root.join("c/x.flac"), "other");
    scan_music(&e);
    hash_pending(&e.lib, &e.cache, &no_cancel()).unwrap();
    let r = relink_orphans(&e.lib, &e.cache).unwrap();
    assert!(r.relinked.is_empty());
    assert_eq!(r.ambiguous, vec!["a/x.flac.xmp".to_string()]);
}
