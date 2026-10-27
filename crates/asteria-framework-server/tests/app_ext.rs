//! The app-extension hooks of the scan: per-item app data and extra files.

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use asteria_framework::journal::Actor;
use asteria_framework::{Library, MediaKind};
use asteria_framework_api::{RpcError, codes};
use asteria_framework_server::{
    AppExtension, CACHE_SCHEMA, Cache, ExtContext, rpc_error, scan, scan_cancellable,
};
use serde_json::{Value, json};

/// Stores the file length as app data and counts the calls to `extract`.
#[derive(Default)]
struct Lengths {
    calls: AtomicUsize,
}

impl AppExtension for Lengths {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
    fn extract(&self, _rel: &str, abs: &Path) -> Option<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some(fs::metadata(abs).unwrap().len().to_string().into_bytes())
    }
    fn wants_extra(&self, rel: &str) -> bool {
        rel.ends_with(".m3u8")
    }
}

struct Env {
    dir: tempfile::TempDir,
    root: PathBuf,
    lib: Library,
    cache: Cache,
    ext: Lengths,
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
        dir,
        root,
        lib,
        cache,
        ext: Lengths::default(),
    }
}

impl Env {
    fn scan(&self) {
        scan(&self.lib, &self.cache, &self.ext).unwrap();
    }
    fn calls(&self) -> usize {
        self.ext.calls.load(Ordering::SeqCst)
    }
    fn red(&self, name: &str) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../asteria-framework/tests/fixtures/red.jpg");
        let to = self.root.join(name);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(src, to).unwrap();
    }
    /// Appends a byte and moves the mtime so (size, mtime) both change.
    fn grow(&self, name: &str) {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(self.root.join(name))
            .unwrap();
        f.write_all(b"x").unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(100))
            .unwrap();
    }
}

#[test]
fn extract_runs_for_new_and_changed_items_only() {
    let e = env();
    e.red("a.jpg");
    e.red("b.jpg");
    e.scan();
    assert_eq!(e.calls(), 2);
    let len = fs::metadata(e.root.join("a.jpg")).unwrap().len();
    assert_eq!(
        e.cache.app_data("a.jpg").unwrap().unwrap(),
        len.to_string().into_bytes()
    );

    e.scan();
    assert_eq!(e.calls(), 2, "an unchanged file is not extracted again");

    e.grow("a.jpg");
    e.scan();
    assert_eq!(e.calls(), 3);
    assert_eq!(
        e.cache.app_data("a.jpg").unwrap().unwrap(),
        (len + 1).to_string().into_bytes()
    );
}

#[test]
fn a_sidecar_only_change_does_not_extract() {
    let e = env();
    e.red("a.jpg");
    e.scan();
    let owner = e.lib.owner();
    e.lib
        .update_sidecar(Path::new("a.jpg"), Actor::User(owner), |sc| {
            sc.description = Some("hello".into());
        })
        .unwrap();
    e.scan();
    assert_eq!(
        e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .sidecar
            .description
            .as_deref(),
        Some("hello")
    );
    assert_eq!(e.calls(), 1);
}

#[test]
fn app_data_goes_with_its_item() {
    let e = env();
    e.red("a.jpg");
    e.scan();
    assert!(e.cache.app_data("a.jpg").unwrap().is_some());
    fs::remove_file(e.root.join("a.jpg")).unwrap();
    e.scan();
    assert_eq!(e.cache.app_data("a.jpg").unwrap(), None);
    assert!(e.cache.all_app_data().unwrap().is_empty());
}

#[test]
fn extras_are_recorded_and_removed() {
    let e = env();
    e.red("a.jpg");
    fs::create_dir(e.root.join("sub")).unwrap();
    fs::write(e.root.join("sub/list.m3u8"), "abc").unwrap();
    e.scan();
    let extras = e.cache.extras().unwrap();
    assert_eq!(extras.len(), 1);
    assert_eq!(extras[0].0, "sub/list.m3u8");
    assert_eq!(extras[0].1.size, 3);
    assert_eq!(e.cache.count().unwrap(), 1, "an extra is not an item");

    fs::write(e.root.join("sub/list.m3u8"), "abcd").unwrap();
    e.scan();
    assert_eq!(e.cache.extras().unwrap()[0].1.size, 4);

    fs::remove_file(e.root.join("sub/list.m3u8")).unwrap();
    e.scan();
    assert!(e.cache.extras().unwrap().is_empty());
}

#[test]
fn hidden_extras_are_ignored() {
    let e = env();
    fs::create_dir(e.root.join(".hidden")).unwrap();
    fs::write(e.root.join(".hidden/x.m3u8"), "abc").unwrap();
    fs::write(e.root.join(".asteria/y.m3u8"), "abc").unwrap();
    fs::write(e.root.join(".z.m3u8"), "abc").unwrap();
    e.scan();
    assert!(e.cache.extras().unwrap().is_empty());
}

#[test]
fn generation_moves_only_on_app_changes() {
    let e = env();
    assert_eq!(e.cache.app_generation(), 0);
    e.red("a.jpg");
    e.scan();
    let g = e.cache.app_generation();
    assert!(g > 0);
    e.scan();
    assert_eq!(e.cache.app_generation(), g, "a scan that changes nothing");

    e.grow("a.jpg");
    e.scan();
    let g2 = e.cache.app_generation();
    assert!(g2 > g);

    fs::write(e.root.join("l.m3u8"), "abc").unwrap();
    e.scan();
    let g3 = e.cache.app_generation();
    assert!(g3 > g2);
    e.scan();
    assert_eq!(e.cache.app_generation(), g3);
}

#[test]
fn schema_4_rebuilds_an_old_cache() {
    let e = env();
    e.red("a.jpg");
    e.scan();
    let id = e.lib.config().id;
    let cache_dir = e.dir.path().join("cache");
    let epoch = e.cache.epoch();
    drop(e.cache);
    assert_eq!(CACHE_SCHEMA, 4);
    {
        let db = redb::Database::open(cache_dir.join("cache.redb")).unwrap();
        let w = db.begin_write().unwrap();
        {
            let def: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("meta");
            let mut t = w.open_table(def).unwrap();
            t.insert("schema", 3u32.to_le_bytes().as_slice()).unwrap();
        }
        w.commit().unwrap();
    }
    let c = Cache::open(&cache_dir, &id).unwrap();
    assert_eq!(c.count().unwrap(), 0);
    assert_ne!(c.epoch(), epoch);
    assert_eq!(c.app_generation(), 0);
}

/// Answers `test.*` methods and tries to answer `folder.list` too.
struct Echo;

impl AppExtension for Echo {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
    fn rpc(
        &self,
        ctx: &ExtContext,
        method: &str,
        params: Value,
    ) -> Option<Result<Value, RpcError>> {
        match method {
            "test.echo" => Some(Ok(json!({
                "params": params,
                "user": ctx.user.to_string(),
                "owner": ctx.owner.to_string(),
            }))),
            "test.missing" => Some(Err(rpc_error(codes::NOT_FOUND, "no such thing"))),
            "folder.list" => Some(Ok(json!({"from": "extension"}))),
            _ => None,
        }
    }
}

fn echo_env() -> common::Env {
    common::env_ext(std::sync::Arc::new(Echo))
}

#[tokio::test]
async fn unknown_method_reaches_the_extension() {
    let e = echo_env();
    let v = common::call(&e.app, &e.owner_token, "test.echo", json!({"n": 1})).await;
    assert_eq!(v["result"]["params"], json!({"n": 1}));
    assert_eq!(v["result"]["user"], e.lib.owner().to_string());
    assert_eq!(v["result"]["owner"], e.lib.owner().to_string());
    // A guest's token makes the guest the context user.
    let v = common::call(&e.app, &e.guest_token, "test.echo", json!({})).await;
    assert_ne!(v["result"]["user"], v["result"]["owner"]);
    assert_eq!(v["result"]["owner"], e.lib.owner().to_string());
}

#[tokio::test]
async fn unhandled_method_is_method_not_found() {
    let e = echo_env();
    let v = common::call(&e.app, &e.owner_token, "test.nothing", json!({})).await;
    assert_eq!(v["error"]["code"], codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn common_methods_never_reach_the_extension() {
    let e = echo_env();
    let v = common::call(&e.app, &e.owner_token, "folder.list", json!({"path": ""})).await;
    assert!(v.get("error").is_none(), "{v}");
    assert!(v["result"].get("from").is_none(), "{v}");
    assert!(v["result"]["items"].is_array(), "{v}");
}

#[tokio::test]
async fn extension_errors_pass_through() {
    let e = echo_env();
    let v = common::call(&e.app, &e.owner_token, "test.missing", json!({})).await;
    assert_eq!(v["error"]["code"], codes::NOT_FOUND);
    assert_eq!(v["error"]["message"], "no such thing");
}

/// Panics while extracting `b.jpg`; stores `ok` for every other file.
struct Panicky;

impl AppExtension for Panicky {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
    fn extract(&self, rel: &str, _abs: &Path) -> Option<Vec<u8>> {
        assert_ne!(rel, "b.jpg", "test extension panics on b.jpg");
        Some(b"ok".to_vec())
    }
}

#[test]
fn a_panicking_extract_does_not_stop_the_scan() {
    let e = env();
    for name in ["a.jpg", "b.jpg", "c.jpg"] {
        e.red(name);
    }
    let report = scan(&e.lib, &e.cache, &Panicky).unwrap();
    assert_eq!(report.added, 3);
    for name in ["a.jpg", "b.jpg", "c.jpg"] {
        assert!(e.cache.get_by_path(name).unwrap().is_some(), "{name}");
    }
    assert_eq!(
        e.cache.app_data("a.jpg").unwrap().as_deref(),
        Some(&b"ok"[..])
    );
    assert_eq!(e.cache.app_data("b.jpg").unwrap(), None);
    assert_eq!(
        e.cache.app_data("c.jpg").unwrap().as_deref(),
        Some(&b"ok"[..])
    );
}

/// Sets the cancel flag from the first `extract`, as Ctrl-C would mid-scan.
struct CancelOnFirst(Arc<AtomicBool>);

impl AppExtension for CancelOnFirst {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
    fn extract(&self, _rel: &str, _abs: &Path) -> Option<Vec<u8>> {
        self.0.store(true, Ordering::Relaxed);
        None
    }
}

#[test]
fn a_cancelled_scan_stops_early_and_removes_nothing() {
    let e = env();
    e.red("old.jpg");
    fs::write(e.root.join("old.m3u8"), "abc").unwrap();
    e.scan();
    fs::remove_file(e.root.join("old.jpg")).unwrap();
    fs::remove_file(e.root.join("old.m3u8")).unwrap();
    e.red("a.jpg");
    e.red("b.jpg");

    let flag = Arc::new(AtomicBool::new(false));
    let report = scan_cancellable(&e.lib, &e.cache, &CancelOnFirst(flag.clone()), &flag).unwrap();
    assert_eq!(report.added, 1);
    assert_eq!(report.removed, 0);
    assert!(e.cache.get_by_path("a.jpg").unwrap().is_some());
    assert!(
        e.cache.get_by_path("b.jpg").unwrap().is_none(),
        "not reached"
    );
    assert!(
        e.cache.get_by_path("old.jpg").unwrap().is_some(),
        "no removal pass"
    );
    assert_eq!(
        e.cache.extras().unwrap().len(),
        1,
        "no extras removal either"
    );

    // A pass that is not cancelled catches up.
    e.scan();
    assert!(e.cache.get_by_path("b.jpg").unwrap().is_some());
    assert!(e.cache.get_by_path("old.jpg").unwrap().is_none());
    assert!(e.cache.extras().unwrap().is_empty());
}
