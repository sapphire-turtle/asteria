use asteria_framework::{LibraryId, MediaKind};
use asteria_framework_api::ItemId;
use asteria_framework_server::{CACHE_SCHEMA, Cache, ItemRecord, SidecarSnapshot, item_id_for};
use uuid::Uuid;

fn lib() -> LibraryId {
    LibraryId(Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap())
}

fn rec(library: &LibraryId, path: &str) -> ItemRecord {
    ItemRecord {
        id: item_id_for(library, path),
        path: path.to_string(),
        kind: MediaKind::Image,
        size: 10,
        mtime_ns: 1,
        hash: None,
        sidecar: SidecarSnapshot::default(),
        sidecar_error: None,
    }
}

#[test]
fn item_id_is_uuid_v5_of_library_and_path() {
    let l = lib();
    assert_eq!(item_id_for(&l, "a/b.jpg"), item_id_for(&l, "a/b.jpg"));
    assert_ne!(item_id_for(&l, "a/b.jpg"), item_id_for(&l, "a/c.jpg"));
    let other = LibraryId(Uuid::new_v4());
    assert_ne!(item_id_for(&l, "a/b.jpg"), item_id_for(&other, "a/b.jpg"));
    let id = item_id_for(&l, "2025/京都旅行/a.jpg");
    let u = Uuid::parse_str(&id.0).unwrap();
    assert_eq!(u.get_version(), Some(uuid::Version::Sha1));
    assert_eq!(id, ItemId("b0a2f96c-dcb1-5b0a-b03e-4c229ab92971".into()));
}

#[test]
fn put_get_remove_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    let r = rec(&l, "x/a.jpg");
    c.put(&r).unwrap();
    assert_eq!(c.get_by_path("x/a.jpg").unwrap().id, r.id);
    assert_eq!(c.get_by_id(&r.id).unwrap().path, "x/a.jpg");
    assert_eq!(c.count(), 1);
    assert_eq!(c.all_paths(), vec!["x/a.jpg".to_string()]);
    c.remove("x/a.jpg").unwrap();
    assert!(c.get_by_path("x/a.jpg").is_none());
    assert!(c.get_by_id(&r.id).is_none());
    assert_eq!(c.count(), 0);
}

#[test]
fn list_folder_pages_and_subfolders() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    for p in [
        "a.jpg",
        "x/b.jpg",
        "x/c.jpg",
        "x/y/d.jpg",
        "z/e.jpg",
        "x.jpg",
    ] {
        c.put(&rec(&l, p)).unwrap();
    }
    let (f, items, next) = c.list_folder("", None, 100);
    assert_eq!(f, ["x", "z"]);
    let paths: Vec<_> = items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(paths, ["a.jpg", "x.jpg"]);
    assert_eq!(next, None);

    let (f, items, next) = c.list_folder("x", None, 1);
    assert_eq!(f, ["y"]);
    assert_eq!(items[0].path, "x/b.jpg");
    assert_eq!(next.as_deref(), Some("x/b.jpg"));

    let (f, items, next) = c.list_folder("x", next.as_deref(), 1);
    assert!(f.is_empty());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, "x/c.jpg");
    assert_eq!(next, None);
}

#[test]
fn schema_mismatch_rebuilds_with_new_epoch() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let epoch = {
        let c = Cache::open(d.path(), &l).unwrap();
        c.put(&rec(&l, "a.jpg")).unwrap();
        c.epoch()
    };
    {
        let db = redb::Database::open(d.path().join("cache.redb")).unwrap();
        let w = db.begin_write().unwrap();
        {
            let def: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("meta");
            let mut t = w.open_table(def).unwrap();
            t.insert("schema", (CACHE_SCHEMA + 1).to_le_bytes().as_slice())
                .unwrap();
        }
        w.commit().unwrap();
    }
    let c = Cache::open(d.path(), &l).unwrap();
    assert_eq!(c.count(), 0);
    assert_ne!(c.epoch(), epoch);
}

#[test]
fn library_mismatch_rebuilds() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    {
        let c = Cache::open(d.path(), &l).unwrap();
        c.put(&rec(&l, "a.jpg")).unwrap();
    }
    let c = Cache::open(d.path(), &LibraryId(Uuid::new_v4())).unwrap();
    assert_eq!(c.count(), 0);
}

#[test]
fn reopen_keeps_data_and_epoch() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let epoch = {
        let c = Cache::open(d.path(), &l).unwrap();
        c.put(&rec(&l, "a.jpg")).unwrap();
        c.epoch()
    };
    let c = Cache::open(d.path(), &l).unwrap();
    assert_eq!(c.count(), 1);
    assert_eq!(c.epoch(), epoch);
}

#[test]
fn corrupt_file_rebuilds() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("cache.redb"), b"garbage bytes not a database").unwrap();
    let c = Cache::open(d.path(), &lib()).unwrap();
    assert_eq!(c.count(), 0);
}

#[test]
fn unprocessable_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let c = Cache::open(d.path(), &lib()).unwrap();
    assert_eq!(c.unprocessable("h", "grid"), None);
    c.mark_unprocessable("h", "grid", "bad codec").unwrap();
    assert_eq!(c.unprocessable("h", "grid").as_deref(), Some("bad codec"));
    assert_eq!(c.unprocessable("h", "other"), None);
}
