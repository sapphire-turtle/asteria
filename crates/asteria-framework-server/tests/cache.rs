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

#[test]
fn set_hash_touches_only_hash_and_checks_size_mtime() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    let mut r = rec(&l, "a.jpg");
    r.sidecar.label = Some("keep".into());
    c.put(&r).unwrap();
    let h = |size, mtime_ns| asteria_framework_server::StoredHash {
        hash: "blake3:aa".into(),
        scope: "file/1".into(),
        file_hash: "blake3:aa".into(),
        size,
        mtime_ns,
    };
    // Stale (size/mtime differ from the record): not written.
    assert!(!c.set_hash("a.jpg", Some(h(11, 1))).unwrap());
    assert!(c.get_by_path("a.jpg").unwrap().unwrap().hash.is_none());
    assert!(c.set_hash("a.jpg", Some(h(10, 1))).unwrap());
    let got = c.get_by_path("a.jpg").unwrap().unwrap();
    assert_eq!(got.hash, Some(h(10, 1)));
    assert_eq!(got.sidecar.label.as_deref(), Some("keep"));
    assert!(!c.set_hash("missing.jpg", None).unwrap());
}

#[test]
fn set_sidecar_touches_only_sidecar_fields() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    c.put(&rec(&l, "a.jpg")).unwrap();
    let snap = SidecarSnapshot {
        owner_rating: Some(5),
        has_sidecar: true,
        ..Default::default()
    };
    assert!(
        c.set_sidecar("a.jpg", snap.clone(), Some("e".into()))
            .unwrap()
    );
    let got = c.get_by_path("a.jpg").unwrap().unwrap();
    assert_eq!(got.sidecar, snap);
    assert_eq!(got.sidecar_error.as_deref(), Some("e"));
    assert_eq!(got.size, 10);
    assert!(!c.set_sidecar("nope.jpg", snap, None).unwrap());
}

fn sh(hash: &str, size: u64, mtime_ns: i128) -> asteria_framework_server::StoredHash {
    asteria_framework_server::StoredHash {
        hash: hash.into(),
        scope: "file/1".into(),
        file_hash: hash.into(),
        size,
        mtime_ns,
    }
}

#[test]
fn hash_index_follows_set_hash_and_remove() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    c.put(&rec(&l, "a.jpg")).unwrap();
    assert!(c.paths_with_hash("blake3:aa", "file/1").unwrap().is_empty());
    assert!(c.set_hash("a.jpg", Some(sh("blake3:aa", 10, 1))).unwrap());
    assert_eq!(c.paths_with_hash("blake3:aa", "file/1").unwrap(), ["a.jpg"]);
    assert!(
        c.paths_with_hash("blake3:aa", "other/1")
            .unwrap()
            .is_empty()
    );
    // A new hash drops the old key.
    assert!(c.set_hash("a.jpg", Some(sh("blake3:bb", 10, 1))).unwrap());
    assert!(c.paths_with_hash("blake3:aa", "file/1").unwrap().is_empty());
    assert_eq!(c.paths_with_hash("blake3:bb", "file/1").unwrap(), ["a.jpg"]);
    // Clearing the hash drops the key.
    assert!(c.set_hash("a.jpg", None).unwrap());
    assert!(c.paths_with_hash("blake3:bb", "file/1").unwrap().is_empty());
    assert!(c.set_hash("a.jpg", Some(sh("blake3:bb", 10, 1))).unwrap());
    c.remove("a.jpg").unwrap();
    assert!(c.paths_with_hash("blake3:bb", "file/1").unwrap().is_empty());
    // put replacing a record with a hash drops the old key too.
    c.put(&rec(&l, "b.jpg")).unwrap();
    c.set_hash("b.jpg", Some(sh("blake3:cc", 10, 1))).unwrap();
    c.put(&rec(&l, "b.jpg")).unwrap();
    assert!(c.paths_with_hash("blake3:cc", "file/1").unwrap().is_empty());
}

#[test]
fn stale_hash_is_not_returned() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    let mut r = rec(&l, "a.jpg");
    r.hash = Some(sh("blake3:aa", 10, 1));
    c.put(&r).unwrap();
    assert_eq!(c.paths_with_hash("blake3:aa", "file/1").unwrap(), ["a.jpg"]);
    r.size = 11;
    c.put(&r).unwrap();
    assert!(c.paths_with_hash("blake3:aa", "file/1").unwrap().is_empty());
    r.size = 10;
    r.mtime_ns = 2;
    c.put(&r).unwrap();
    assert!(c.paths_with_hash("blake3:aa", "file/1").unwrap().is_empty());
}

#[test]
fn same_payload_two_paths() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    let mut recs = Vec::new();
    for p in ["z.jpg", "a/b.jpg", "m.jpg"] {
        let mut r = rec(&l, p);
        r.hash = Some(sh("blake3:aa", 10, 1));
        recs.push(r);
    }
    recs[2].hash = Some(sh("blake3:ab", 10, 1));
    c.put_many(&recs).unwrap();
    assert_eq!(
        c.paths_with_hash("blake3:aa", "file/1").unwrap(),
        ["a/b.jpg", "z.jpg"]
    );
    c.remove_many(&["z.jpg".to_string()]).unwrap();
    assert_eq!(
        c.paths_with_hash("blake3:aa", "file/1").unwrap(),
        ["a/b.jpg"]
    );
}

#[test]
fn schema_2_cache_is_rebuilt() {
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
            t.insert("schema", 2u32.to_le_bytes().as_slice()).unwrap();
        }
        w.commit().unwrap();
    }
    let c = Cache::open(d.path(), &l).unwrap();
    assert_eq!(c.count().unwrap(), 0);
    assert_ne!(c.epoch(), epoch);
}

#[test]
fn leftover_key_of_undecodable_record_is_not_returned() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    {
        let c = Cache::open(d.path(), &l).unwrap();
        let mut r = rec(&l, "a.jpg");
        r.hash = Some(sh("blake3:aa", 10, 1));
        c.put(&r).unwrap();
    }
    {
        let db = redb::Database::open(d.path().join("cache.redb")).unwrap();
        let w = db.begin_write().unwrap();
        {
            let def: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("items");
            let mut t = w.open_table(def).unwrap();
            t.insert("a.jpg", b"garbage".as_slice()).unwrap();
        }
        w.commit().unwrap();
    }
    let c = Cache::open(d.path(), &l).unwrap();
    let mut r = rec(&l, "a.jpg");
    r.hash = Some(sh("blake3:bb", 10, 1));
    c.put(&r).unwrap();
    assert!(c.paths_with_hash("blake3:aa", "file/1").unwrap().is_empty());
    assert_eq!(c.paths_with_hash("blake3:bb", "file/1").unwrap(), ["a.jpg"]);
}

#[test]
fn set_hash_many_and_put_many_replace_changed_hash() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    let recs: Vec<_> = ["a.jpg", "b.jpg"]
        .iter()
        .map(|p| {
            let mut r = rec(&l, p);
            r.hash = Some(sh("blake3:aa", 10, 1));
            r
        })
        .collect();
    c.put_many(&recs).unwrap();
    let n = c
        .set_hash_many(&[
            ("a.jpg".to_string(), Some(sh("blake3:bb", 10, 1))),
            ("b.jpg".to_string(), None),
        ])
        .unwrap();
    assert_eq!(n, 2);
    assert!(c.paths_with_hash("blake3:aa", "file/1").unwrap().is_empty());
    assert_eq!(c.paths_with_hash("blake3:bb", "file/1").unwrap(), ["a.jpg"]);
    let mut r = rec(&l, "a.jpg");
    r.hash = Some(sh("blake3:cc", 10, 1));
    c.put_many(&[r]).unwrap();
    assert!(c.paths_with_hash("blake3:bb", "file/1").unwrap().is_empty());
    assert_eq!(c.paths_with_hash("blake3:cc", "file/1").unwrap(), ["a.jpg"]);
}

#[test]
fn put_many_with_app_data_commits_records_and_app_data_together() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let recs: Vec<_> = ["a.jpg", "b.jpg"].iter().map(|p| rec(&l, p)).collect();
    {
        let c = Cache::open(d.path(), &l).unwrap();
        c.put_many_with_app_data(
            &[rec(&l, "b.jpg")],
            &[("b.jpg".into(), Some(b"old".to_vec()))],
        )
        .unwrap();
        let g = c.app_generation();
        c.put_many_with_app_data(
            &recs,
            &[
                ("a.jpg".into(), Some(b"tags".to_vec())),
                ("b.jpg".into(), None),
            ],
        )
        .unwrap();
        assert_eq!(c.app_generation(), g + 1, "one batch, one generation step");
        // A batch that changes no app data leaves the generation alone.
        c.put_many_with_app_data(&recs, &[("b.jpg".into(), None)])
            .unwrap();
        assert_eq!(c.app_generation(), g + 1);
        c.flush().unwrap();
    }
    let c = Cache::open(d.path(), &l).unwrap();
    assert_eq!(c.count().unwrap(), 2);
    assert_eq!(c.app_data("a.jpg").unwrap().as_deref(), Some(&b"tags"[..]));
    assert_eq!(c.app_data("b.jpg").unwrap(), None);
}

#[test]
fn move_item_carries_the_record_hash_and_app_data() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    let h = sh("abc", 10, 1);
    let mut r = rec(&l, "a.jpg");
    r.hash = Some(h.clone());
    c.put_many_with_app_data(&[r], &[("a.jpg".into(), Some(b"tags".to_vec()))])
        .unwrap();
    let before = c.get_by_path("a.jpg").unwrap().unwrap();
    let gen_before = c.app_generation();

    let moved = c.move_item("a.jpg", "sub/c.jpg").unwrap().unwrap();
    assert_eq!(moved.path, "sub/c.jpg");
    assert_eq!(moved.id, item_id_for(&l, "sub/c.jpg"));
    assert_eq!(moved.hash, before.hash);
    assert_eq!(moved.size, before.size);
    assert_eq!(moved.mtime_ns, before.mtime_ns);
    assert!(c.get_by_path("a.jpg").unwrap().is_none());
    assert!(c.get_by_id(&before.id).unwrap().is_none());
    assert_eq!(c.get_by_id(&moved.id).unwrap().unwrap(), moved);
    assert_eq!(
        c.paths_with_hash(&h.hash, &h.scope).unwrap(),
        vec!["sub/c.jpg".to_string()]
    );
    assert_eq!(c.app_data("sub/c.jpg").unwrap(), Some(b"tags".to_vec()));
    assert_eq!(c.app_data("a.jpg").unwrap(), None);
    assert!(c.app_generation() > gen_before);
}

#[test]
fn move_item_replaces_an_existing_destination() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    c.put(&rec(&l, "a.jpg")).unwrap();
    let mut other = rec(&l, "b.jpg");
    other.hash = Some(sh("old", 10, 1));
    c.put(&other).unwrap();
    c.move_item("a.jpg", "b.jpg").unwrap().unwrap();
    assert_eq!(c.count().unwrap(), 1);
    assert!(c.paths_with_hash("old", "file/1").unwrap().is_empty());
}

#[test]
fn moving_a_missing_record_does_nothing() {
    let d = tempfile::tempdir().unwrap();
    let c = Cache::open(d.path(), &lib()).unwrap();
    let r0 = c.revision();
    assert_eq!(c.move_item("nope.jpg", "x.jpg").unwrap(), None);
    assert_eq!(c.revision(), r0);
}

#[test]
fn revision_moves_with_item_changes_only() {
    let d = tempfile::tempdir().unwrap();
    let l = lib();
    let c = Cache::open(d.path(), &l).unwrap();
    assert_eq!(c.revision(), 0);
    let r0 = c.revision();
    c.put(&rec(&l, "a.jpg")).unwrap();
    assert_eq!(c.revision(), r0 + 1);
    c.set_sidecar("a.jpg", SidecarSnapshot::default(), None)
        .unwrap();
    assert_eq!(c.revision(), r0 + 2);
    c.set_sidecar("gone.jpg", SidecarSnapshot::default(), None)
        .unwrap();
    assert_eq!(c.revision(), r0 + 2);
    c.put_extras(&[(
        "p.m3u8".into(),
        asteria_framework_server::ExtraRecord {
            size: 1,
            mtime_ns: 1,
        },
    )])
    .unwrap();
    assert_eq!(c.revision(), r0 + 2);
    c.remove("a.jpg").unwrap();
    assert_eq!(c.revision(), r0 + 3);
    c.remove("a.jpg").unwrap();
    assert_eq!(c.revision(), r0 + 3);
    c.put(&rec(&l, "m.jpg")).unwrap();
    c.move_item("m.jpg", "n.jpg").unwrap();
    assert_eq!(c.revision(), r0 + 5);
}
