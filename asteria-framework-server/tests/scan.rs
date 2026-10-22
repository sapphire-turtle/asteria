use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use asteria_framework::{Library, MediaKind};
use asteria_framework_server::{Cache, MediaFilter, ScanReport, scan};

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

fn run(e: &Env) -> ScanReport {
    scan(&e.lib, &e.cache, &Gallery).unwrap()
}

fn red(to: &Path) {
    fs::copy(fixture("red.jpg"), to).unwrap();
}

fn paths(e: &Env) -> Vec<String> {
    let mut p = e.cache.all_paths().unwrap();
    p.sort();
    p
}

#[test]
fn scans_images_and_videos_only() {
    let e = env();
    red(&e.root.join("red.jpg"));
    fs::write(e.root.join("clip.mov"), b"not really a movie").unwrap();
    fs::copy(fixture("sine440.mp3"), e.root.join("song.mp3")).unwrap();
    fs::write(e.root.join("notes.txt"), b"hello").unwrap();
    let r = run(&e);
    assert_eq!(r.added, 2);
    assert_eq!(paths(&e), vec!["clip.mov", "red.jpg"]);
}

#[test]
fn skips_hidden_meta_and_sidecars() {
    let e = env();
    red(&e.root.join("a.jpg"));
    fs::write(e.root.join("a.jpg.xmp"), b"x").unwrap();
    fs::create_dir(e.root.join(".hidden")).unwrap();
    red(&e.root.join(".hidden/a.jpg"));
    red(&e.root.join(".b.jpg"));
    red(&e.root.join(".asteria/c.jpg"));
    run(&e);
    assert_eq!(paths(&e), vec!["a.jpg"]);
}

#[test]
fn rescan_detects_add_update_remove() {
    let e = env();
    red(&e.root.join("keep.jpg"));
    red(&e.root.join("change.jpg"));
    red(&e.root.join("gone.jpg"));
    let r = run(&e);
    assert_eq!((r.added, r.updated, r.removed), (3, 0, 0));
    assert_eq!(run(&e), ScanReport::default());

    red(&e.root.join("new.jpg"));
    let f = fs::File::options()
        .write(true)
        .open(e.root.join("change.jpg"))
        .unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(100))
        .unwrap();
    fs::remove_file(e.root.join("gone.jpg")).unwrap();
    let r = run(&e);
    assert_eq!((r.added, r.updated, r.removed), (1, 1, 1));
    assert_eq!(paths(&e), vec!["change.jpg", "keep.jpg", "new.jpg"]);
}

#[test]
fn malformed_sidecar_is_listed_with_error() {
    let e = env();
    red(&e.root.join("red.jpg"));
    fs::write(e.root.join("red.jpg.xmp"), b"<not xml").unwrap();
    run(&e);
    let rec = e.cache.get_by_path("red.jpg").unwrap().unwrap();
    assert!(rec.sidecar_error.is_some());
    assert!(!rec.sidecar.has_sidecar);
}

#[cfg(unix)]
#[test]
fn symlink_outside_root_is_ignored() {
    let e = env();
    let outside = e._dir.path().join("outside.jpg");
    red(&outside);
    red(&e.root.join("inside.jpg"));
    std::os::unix::fs::symlink(&outside, e.root.join("out.jpg")).unwrap();
    std::os::unix::fs::symlink(e.root.join("inside.jpg"), e.root.join("in.jpg")).unwrap();
    run(&e);
    assert_eq!(paths(&e), vec!["in.jpg", "inside.jpg"]);
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_name_is_skipped() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let e = env();
    red(&e.root.join(OsStr::from_bytes(b"\xff.jpg")));
    red(&e.root.join("ok.jpg"));
    let r = run(&e);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(paths(&e), vec!["ok.jpg"]);
}

#[test]
fn scan_does_not_touch_tmp_files() {
    let e = env();
    red(&e.root.join("a.jpg"));
    let tmp = e.root.join(".a.jpg.xmp.asteria-tmp");
    fs::write(&tmp, b"junk").unwrap();
    run(&e);
    assert!(tmp.exists());
}

#[test]
fn hash_survives_unchanged_rescan_and_is_dropped_on_change() {
    use asteria_framework_server::StoredHash;
    let e = env();
    red(&e.root.join("a.jpg"));
    run(&e);
    let mut rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
    rec.hash = Some(StoredHash {
        hash: "h".into(),
        scope: "s".into(),
        size: rec.size,
        mtime_ns: rec.mtime_ns,
    });
    e.cache.put(&rec).unwrap();
    assert_eq!(run(&e), ScanReport::default());
    assert!(
        e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .hash
            .is_some()
    );
    let f = fs::File::options()
        .write(true)
        .open(e.root.join("a.jpg"))
        .unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(100))
        .unwrap();
    assert_eq!(run(&e).updated, 1);
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
fn large_tree_scan_is_reasonable() {
    let e = env();
    for d in 0..50 {
        let dir = e.root.join(format!("d{d}"));
        fs::create_dir(&dir).unwrap();
        for i in 0..100 {
            red(&dir.join(format!("{i}.jpg")));
        }
    }
    let t = Instant::now();
    let r = run(&e);
    let first = t.elapsed();
    assert_eq!(r.added, 5000);
    assert!(first < Duration::from_secs(30), "took {first:?}");
    let t = Instant::now();
    assert_eq!(run(&e), ScanReport::default());
    let second = t.elapsed();
    assert!(
        second < first,
        "rescan {second:?} not faster than first {first:?}"
    );
}

#[test]
fn sidecar_only_edit_is_an_update() {
    use asteria_framework::journal::Actor;
    let e = env();
    red(&e.root.join("a.jpg"));
    run(&e);
    assert!(
        !e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .sidecar
            .has_sidecar
    );
    let owner = e.lib.owner();
    e.lib
        .update_sidecar(Path::new("a.jpg"), Actor::User(owner), |sc| {
            sc.description = Some("hello".into());
        })
        .unwrap();
    let r = run(&e);
    assert_eq!((r.added, r.updated, r.removed), (0, 1, 0));
    let rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
    assert_eq!(rec.sidecar.description.as_deref(), Some("hello"));
    assert!(rec.sidecar.has_sidecar);
}

#[test]
fn fixing_malformed_sidecar_clears_error() {
    let e = env();
    red(&e.root.join("a.jpg"));
    fs::write(e.root.join("a.jpg.xmp"), b"<not xml").unwrap();
    run(&e);
    assert!(
        e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .sidecar_error
            .is_some()
    );
    fs::copy(fixture("xmp/asteria_full.xmp"), e.root.join("a.jpg.xmp")).unwrap();
    let r = run(&e);
    assert_eq!(r.updated, 1);
    let rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
    assert!(rec.sidecar_error.is_none());
    assert!(rec.sidecar.has_sidecar);
}

#[cfg(unix)]
#[test]
fn symlink_into_hidden_or_sidecar_is_rejected() {
    use std::os::unix::fs::symlink;
    let e = env();
    red(&e.root.join("a.jpg"));
    fs::write(e.root.join("a.jpg.xmp"), b"x").unwrap();
    fs::create_dir(e.root.join(".hidden")).unwrap();
    red(&e.root.join(".hidden/h.jpg"));
    red(&e.root.join(".asteria/m.jpg"));
    symlink(e.root.join(".asteria/m.jpg"), e.root.join("l1.jpg")).unwrap();
    symlink(e.root.join(".hidden/h.jpg"), e.root.join("l2.jpg")).unwrap();
    symlink(e.root.join("a.jpg.xmp"), e.root.join("l3.jpg")).unwrap();
    run(&e);
    assert_eq!(paths(&e), vec!["a.jpg"]);
}

#[cfg(unix)]
#[test]
fn symlinked_directory_is_not_followed() {
    let e = env();
    fs::create_dir(e.root.join("real")).unwrap();
    red(&e.root.join("real/a.jpg"));
    std::os::unix::fs::symlink(e.root.join("real"), e.root.join("link")).unwrap();
    run(&e);
    assert_eq!(paths(&e), vec!["real/a.jpg"]);
}

#[cfg(unix)]
#[test]
fn records_under_unreadable_subtree_are_retained() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    fs::create_dir(e.root.join("sub")).unwrap();
    red(&e.root.join("sub/a.jpg"));
    red(&e.root.join("b.jpg"));
    run(&e);
    let sub = e.root.join("sub");
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&sub).is_ok() {
        // Running as root: permissions are not enforced.
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let r = run(&e);
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(r.removed, 0);
    assert!(r.skipped.iter().any(|(p, _)| p == "sub"), "{:?}", r.skipped);
    assert_eq!(paths(&e), vec!["b.jpg", "sub/a.jpg"]);
}

#[test]
fn removing_more_than_one_batch_works() {
    let e = env();
    for i in 0..1100 {
        red(&e.root.join(format!("{i}.jpg")));
    }
    assert_eq!(run(&e).added, 1100);
    for i in 0..1100 {
        fs::remove_file(e.root.join(format!("{i}.jpg"))).unwrap();
    }
    let r = run(&e);
    assert_eq!(r.removed, 1100);
    assert!(paths(&e).is_empty());
}
