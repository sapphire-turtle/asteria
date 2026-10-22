use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use asteria_framework::journal::Journal;
use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_api::{ItemId, MediaUpdateParams};
use asteria_framework_server::{
    Cache, MediaFilter, StoredHash, WriteError, Writer, scan, spawn_writer,
};
use grain_id::GrainId;

struct Gallery;
impl MediaFilter for Gallery {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    lib: Arc<Library>,
    cache: Arc<Cache>,
    writer: Writer,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Arc::new(Library::init(&root, "owner").unwrap());
    let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../asteria-framework/tests/fixtures/red.jpg"),
        root.join("a.jpg"),
    )
    .unwrap();
    scan(&lib, &cache, &Gallery).unwrap();
    let (writer, _h) = spawn_writer(lib.clone(), cache.clone());
    Env {
        _dir: dir,
        root,
        lib,
        cache,
        writer,
    }
}

fn id(e: &Env) -> ItemId {
    e.cache.get_by_path("a.jpg").unwrap().unwrap().id
}

fn params(id: ItemId) -> MediaUpdateParams {
    MediaUpdateParams {
        id,
        rating: None,
        subjects: None,
        hierarchical_subjects: None,
        description: None,
        label: None,
    }
}

#[tokio::test]
async fn update_rating_and_tags_writes_sidecar_and_cache() {
    let e = env();
    let mut p = params(id(&e));
    p.rating = Some(4);
    p.subjects = Some(vec!["cat".into()]);
    let owner = e.lib.owner();
    let rec = e.writer.update(owner, p).await.unwrap();
    assert_eq!(rec.sidecar.owner_rating, Some(4));
    assert_eq!(rec.sidecar.subjects, vec!["cat".to_string()]);
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_eq!(sc.rating.unwrap().value, 4);
    assert!(sc.subjects.contains("cat"));
    assert!(sc.content_hash.is_some());
    assert_eq!(e.cache.get_by_path("a.jpg").unwrap().unwrap(), rec);
}

#[tokio::test]
async fn non_owner_rating_goes_to_user_ratings() {
    let e = env();
    let other = UserId(GrainId::random());
    let mut p = params(id(&e));
    p.rating = Some(2);
    let rec = e.writer.update(other, p).await.unwrap();
    assert_eq!(rec.sidecar.owner_rating, None);
    assert_eq!(rec.sidecar.user_ratings.get(&other.to_string()), Some(&2));
}

#[tokio::test]
async fn read_only_item_is_refused_untouched() {
    let e = env();
    let xmp = e.root.join("a.jpg.xmp");
    fs::write(&xmp, b"<not xmp").unwrap();
    scan(&e.lib, &e.cache, &Gallery).unwrap();
    let mut p = params(id(&e));
    p.rating = Some(3);
    let r = e.writer.update(e.lib.owner(), p).await;
    assert!(matches!(r, Err(WriteError::ReadOnly(_))), "{r:?}");
    assert_eq!(fs::read(&xmp).unwrap(), b"<not xmp");
}

#[tokio::test]
async fn deleted_file_is_not_found() {
    let e = env();
    let mut p = params(id(&e));
    p.label = Some(Some("x".into()));
    fs::remove_file(e.root.join("a.jpg")).unwrap();
    let r = e.writer.update(e.lib.owner(), p).await;
    assert_eq!(r, Err(WriteError::NotFound));
    let r = e
        .writer
        .update(e.lib.owner(), params(ItemId("nope".into())))
        .await;
    assert_eq!(r, Err(WriteError::NotFound));
}

#[tokio::test]
async fn control_chars_are_invalid() {
    let e = env();
    for f in 0..4 {
        let mut p = params(id(&e));
        match f {
            0 => p.label = Some(Some("\u{1}".into())),
            1 => p.description = Some(Some("a\u{FFFE}".into())),
            2 => p.subjects = Some(vec!["ok".into(), "b\u{0}".into()]),
            _ => p.hierarchical_subjects = Some(vec!["a|\u{1f}".into()]),
        }
        let r = e.writer.update(e.lib.owner(), p).await;
        assert!(matches!(r, Err(WriteError::Invalid(_))), "{r:?}");
    }
    assert!(!e.root.join("a.jpg.xmp").exists());
    let mut p = params(id(&e));
    p.label = Some(Some("tab\tnl\n".into()));
    e.writer.update(e.lib.owner(), p).await.unwrap();
    let mut p = params(id(&e));
    p.rating = Some(9);
    assert!(matches!(
        e.writer.update(e.lib.owner(), p).await,
        Err(WriteError::Invalid(_))
    ));
}

#[tokio::test]
async fn concurrent_updates_are_serialized() {
    let e = env();
    let owner = e.lib.owner();
    let before = journal_len(&e);
    let mut hs = Vec::new();
    for i in 0..20 {
        let w = e.writer.clone();
        let mut p = params(id(&e));
        p.label = Some(Some(format!("l{i}")));
        p.subjects = Some(vec![format!("s{i}")]);
        hs.push(tokio::spawn(async move { w.update(owner, p).await }));
    }
    for h in hs {
        h.await.unwrap().unwrap();
    }
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    let cached = e.cache.get_by_path("a.jpg").unwrap().unwrap().sidecar;
    assert_eq!(sc.label, cached.label);
    assert_eq!(sc.subjects.len(), 1);
    assert_eq!(journal_len(&e), before + 20);
}

#[tokio::test]
async fn cached_hash_avoids_rehash() {
    let e = env();
    let rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
    let fake = format!("blake3:{}", "ab".repeat(32));
    assert!(
        e.cache
            .set_hash(
                "a.jpg",
                Some(StoredHash {
                    hash: fake.clone(),
                    scope: "media-payload/1".into(),
                    size: rec.size,
                    mtime_ns: rec.mtime_ns,
                }),
            )
            .unwrap()
    );
    let mut p = params(rec.id);
    p.label = Some(Some("x".into()));
    e.writer.update(e.lib.owner(), p).await.unwrap();
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_eq!(sc.content_hash.unwrap().0.to_string(), fake);
}

fn journal_len(e: &Env) -> usize {
    Journal::new(&e.lib).read_all().unwrap().len()
}

fn fake_hash(e: &Env) -> String {
    let rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
    let fake = format!("blake3:{}", "cd".repeat(32));
    e.cache
        .set_hash(
            "a.jpg",
            Some(StoredHash {
                hash: fake.clone(),
                scope: "media-payload/1".into(),
                size: rec.size,
                mtime_ns: rec.mtime_ns,
            }),
        )
        .unwrap();
    fake
}

#[tokio::test]
async fn rating_only_uses_cached_hash() {
    let e = env();
    let fake = fake_hash(&e);
    let mut p = params(id(&e));
    p.rating = Some(5);
    e.writer.update(e.lib.owner(), p).await.unwrap();
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_eq!(sc.content_hash.unwrap().0.to_string(), fake);
    assert_eq!(sc.rating.unwrap().value, 5);
}

#[tokio::test]
async fn rating_and_label_is_one_journal_entry() {
    let e = env();
    let before = journal_len(&e);
    let mut p = params(id(&e));
    p.rating = Some(3);
    p.label = Some(Some("x".into()));
    e.writer.update(e.lib.owner(), p).await.unwrap();
    assert_eq!(journal_len(&e), before + 1);
}

#[tokio::test]
async fn stale_cached_hash_is_not_used() {
    let e = env();
    let fake = fake_hash(&e);
    // Modify the file after scan and hashing: new size and mtime.
    let mut bytes = fs::read(e.root.join("a.jpg")).unwrap();
    bytes.push(0);
    fs::write(e.root.join("a.jpg"), bytes).unwrap();
    let mut p = params(id(&e));
    p.label = Some(Some("x".into()));
    e.writer.update(e.lib.owner(), p).await.unwrap();
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_ne!(sc.content_hash.unwrap().0.to_string(), fake);
}

#[tokio::test]
async fn malformed_sidecar_at_write_time_sets_error() {
    let e = env();
    // Sidecar broken after the scan: the cache does not know yet.
    fs::write(e.root.join("a.jpg.xmp"), b"<not xmp").unwrap();
    let mut p = params(id(&e));
    p.label = Some(Some("x".into()));
    let r = e.writer.update(e.lib.owner(), p.clone()).await;
    assert!(matches!(r, Err(WriteError::ReadOnly(_))), "{r:?}");
    assert!(
        e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .sidecar_error
            .is_some()
    );
    assert_eq!(fs::read(e.root.join("a.jpg.xmp")).unwrap(), b"<not xmp");
}

#[tokio::test]
async fn writer_does_not_touch_cached_hash() {
    let e = env();
    let mut p = params(id(&e));
    p.label = Some(Some("x".into()));
    let rec = e.writer.update(e.lib.owner(), p).await.unwrap();
    assert_eq!(rec.hash, None);
    assert_eq!(e.cache.get_by_path("a.jpg").unwrap().unwrap().hash, None);
}

#[tokio::test]
async fn shutdown_drains_all_queued_jobs() {
    let e = env();
    let owner = e.lib.owner();
    let (w, h) = spawn_writer(e.lib.clone(), e.cache.clone());
    let mut tasks = Vec::new();
    for i in 0..5 {
        let w = w.clone();
        let mut p = params(id(&e));
        p.subjects = Some(vec![format!("t{i}")]);
        tasks.push(tokio::spawn(async move { w.update(owner, p).await }));
    }
    // Let every task enqueue its job before dropping our handle.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    drop(w);
    h.await.unwrap();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_eq!(sc.subjects.len(), 1);
    assert_eq!(journal_len(&e), 5);
}

#[tokio::test]
async fn stopped_writer_errors() {
    let e = env();
    let (w, h) = spawn_writer(e.lib.clone(), e.cache.clone());
    h.abort();
    let _ = h.await;
    assert_eq!(
        w.relink_orphans().await,
        Err(WriteError::Internal("writer stopped".into()))
    );
}
