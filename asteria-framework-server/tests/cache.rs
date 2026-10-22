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
    assert_eq!(c.get_by_path("x/a.jpg").unwrap().unwrap().id, r.id);
    assert_eq!(c.get_by_id(&r.id).unwrap().unwrap().path, "x/a.jpg");
    assert_eq!(c.count().unwrap(), 1);
    assert_eq!(c.all_paths().unwrap(), vec!["x/a.jpg".to_string()]);
    c.remove("x/a.jpg").unwrap();
    assert!(c.get_by_path("x/a.jpg").unwrap().is_none());
    assert!(c.get_by_id(&r.id).unwrap().is_none());
    assert_eq!(c.count().unwrap(), 0);
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
    let (f, items, next) = c.list_folder("", None, 100).unwrap();
    assert_eq!(f, ["x", "z"]);
    let paths: Vec<_> = items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(paths, ["a.jpg", "x.jpg"]);
    assert_eq!(next, None);

    let (f, items, next) = c.list_folder("x", None, 1).unwrap();
    assert_eq!(f, ["y"]);
    assert_eq!(items[0].path, "x/b.jpg");
    assert_eq!(next.as_deref(), Some("x/b.jpg"));

    let (f, items, next) = c.list_folder("x", next.as_deref(), 1).unwrap();
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
    assert_eq!(c.count().unwrap(), 0);
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
    assert_eq!(c.count().unwrap(), 0);
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
    assert_eq!(c.count().unwrap(), 1);
    assert_eq!(c.epoch(), epoch);
}

#[test]
fn corrupt_file_rebuilds() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("cache.redb"), b"garbage bytes not a database").unwrap();
    let c = Cache::open(d.path(), &lib()).unwrap();
    assert_eq!(c.count().unwrap(), 0);
}

#[test]
fn unprocessable_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let c = Cache::open(d.path(), &lib()).unwrap();
    assert_eq!(c.unprocessable("h", "grid").unwrap(), None);
    c.mark_unprocessable("h", "grid", "bad codec").unwrap();
    assert_eq!(
        c.unprocessable("h", "grid").unwrap().as_deref(),
        Some("bad codec")
    );
    assert_eq!(c.unprocessable("h", "other").unwrap(), None);
}

fn put_paths(c: &Cache, l: &LibraryId, paths: &[&str]) {
    for p in paths {
        c.put(&rec(l, p)).unwrap();
    }
}

#[test]
fn second_open_while_first_alive_errors_and_keeps_data() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    c.put(&rec(&l, "a.jpg")).unwrap();
    assert!(Cache::open(d.path(), &l).is_err());
    assert_eq!(c.count().unwrap(), 1);
    assert!(c.get_by_path("a.jpg").unwrap().is_some());
}

#[test]
fn put_many_then_flush_survives_reopen() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    {
        let c = Cache::open(d.path(), &l).unwrap();
        let recs: Vec<_> = ["a.jpg", "b/c.jpg"].iter().map(|p| rec(&l, p)).collect();
        c.put_many(&recs).unwrap();
        c.flush().unwrap();
    }
    let c = Cache::open(d.path(), &l).unwrap();
    assert_eq!(c.count().unwrap(), 2);
    c.remove_many(&["a.jpg".to_string(), "b/c.jpg".to_string()])
        .unwrap();
    assert_eq!(c.count().unwrap(), 0);
}

#[test]
fn prefix_siblings_and_japanese_paths() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    put_paths(
        &c,
        &l,
        &[
            "x-y/a.jpg",
            "x.jpg",
            "x/b.jpg",
            "京都/写真.jpg",
            "京都/奈良/b.jpg",
        ],
    );
    let (f, items, _) = c.list_folder("x", None, 10).unwrap();
    assert!(f.is_empty());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, "x/b.jpg");
    let (f, items, _) = c.list_folder("京都", None, 10).unwrap();
    assert_eq!(f, ["奈良"]);
    assert_eq!(items[0].path, "京都/写真.jpg");
}

#[test]
fn rename_and_overwrite_keep_id_index_consistent() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    let old = rec(&l, "a.jpg");
    c.put(&old).unwrap();
    c.remove("a.jpg").unwrap();
    let new = rec(&l, "b.jpg");
    c.put(&new).unwrap();
    assert!(c.get_by_id(&old.id).unwrap().is_none());
    assert_eq!(c.get_by_id(&new.id).unwrap().unwrap().path, "b.jpg");
    let mut again = new.clone();
    again.size = 99;
    c.put(&again).unwrap();
    assert_eq!(c.count().unwrap(), 1);
    assert_eq!(c.get_by_id(&new.id).unwrap().unwrap().size, 99);
}

#[test]
fn cursor_outside_folder_is_empty_page() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    put_paths(&c, &l, &["a.jpg", "x/b.jpg", "z/c.jpg"]);
    for cur in ["a.jpg", "z/c.jpg"] {
        let (f, items, next) = c.list_folder("x", Some(cur), 10).unwrap();
        assert!(f.is_empty() && items.is_empty() && next.is_none());
    }
}

#[test]
fn folder_names_sorted_and_limit_clamped() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    put_paths(&c, &l, &["y-z/a.jpg", "y/a.jpg", "a.jpg", "b.jpg"]);
    let (f, items, next) = c.list_folder("", None, 0).unwrap();
    assert_eq!(f, ["y", "y-z"]);
    assert_eq!(items.len(), 1);
    assert_eq!(next.as_deref(), Some("a.jpg"));
}

#[test]
fn invalid_folder_arguments_rejected() {
    let d = tempfile::tempdir().unwrap();
    let c = Cache::open(d.path(), &lib()).unwrap();
    for bad in ["/x", "x/", ".", "..", "a//b", "a/../b", "a/./b"] {
        assert!(c.list_folder(bad, None, 10).is_err(), "{bad}");
    }
    assert!(c.list_folder("", None, 10).is_ok());
}
