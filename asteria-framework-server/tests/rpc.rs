use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_api::codes;
use asteria_framework_server::{
    AppState, Cache, MediaFilter, ServerConfig, StoredHash, TokenEntry, Tokens, new_token, router,
    scan, spawn_writer, token_hash,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use grain_id::GrainId;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

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
    app: Router,
    owner_token: String,
    guest: UserId,
    guest_token: String,
    _rescan_rx: tokio::sync::mpsc::Receiver<()>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Arc::new(Library::init(&root, "owner").unwrap());
    let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
    let red =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../asteria-framework/tests/fixtures/red.jpg");
    fs::copy(&red, root.join("a.jpg")).unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::copy(&red, root.join("sub/b.jpg")).unwrap();
    scan(&lib, &cache, &Gallery).unwrap();
    let (writer, _h) = spawn_writer(lib.clone(), cache.clone());

    let owner_token = new_token();
    let guest_token = new_token();
    let guest = UserId(GrainId::random());
    let cfg = ServerConfig {
        library: root.clone(),
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: vec![
            TokenEntry {
                user: lib.owner(),
                hash: token_hash(&owner_token),
            },
            TokenEntry {
                user: guest,
                hash: token_hash(&guest_token),
            },
        ],
    };
    let (rescan, rescan_rx) = tokio::sync::mpsc::channel(1);
    let state = AppState::new(
        lib.clone(),
        cache.clone(),
        writer,
        Arc::new(Tokens::from_config(&cfg)),
        "asteria-gallery",
        1,
        Arc::new(AtomicBool::new(false)),
        rescan,
        Arc::new(Gallery),
    );
    Env {
        _dir: dir,
        root,
        lib,
        cache,
        app: router(state),
        owner_token,
        guest,
        guest_token,
        _rescan_rx: rescan_rx,
    }
}

async fn post_raw(app: &Router, token: Option<&str>, body: &str) -> (StatusCode, Vec<u8>) {
    let mut req = Request::post("/rpc");
    if let Some(t) = token {
        req = req.header("authorization", t);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (
        status,
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

async fn call(app: &Router, token: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}).to_string();
    let (status, bytes) = post_raw(app, Some(&format!("Bearer {token}")), &body).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 7);
    v
}

fn item_id(e: &Env, path: &str) -> String {
    e.cache.get_by_path(path).unwrap().unwrap().id.0
}

#[tokio::test]
async fn missing_or_wrong_token_is_401() {
    let e = env();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"server.info"}"#;
    let good = format!("Bearer {}", e.owner_token);
    for header in [
        None,
        Some("Bearer nope"),
        Some("Basic abc"),
        Some(e.owner_token.as_str()),
        Some("bearer x"),
    ] {
        let (status, bytes) = post_raw(&e.app, header, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{header:?}");
        assert!(bytes.is_empty());
    }
    let (status, _) = post_raw(&e.app, Some(&good), body).await;
    assert_eq!(status, StatusCode::OK);
    // The whole router is guarded, including unknown paths.
    let resp = e
        .app
        .clone()
        .oneshot(Request::get("/other").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn server_info_reports_app_and_epoch() {
    let e = env();
    let v = call(&e.app, &e.owner_token, "server.info", json!({})).await;
    assert_eq!(v["result"]["app"], "asteria-gallery");
    assert_eq!(v["result"]["app_api"], 1);
    assert_eq!(v["result"]["server_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["result"]["epoch"], e.cache.epoch());
    let v = call(&e.app, &e.owner_token, "library.info", json!({})).await;
    assert_eq!(v["result"]["item_count"], 2);
    assert_eq!(v["result"]["scanning"], false);
    assert_eq!(v["result"]["owner"], "owner");
    let v = call(&e.app, &e.owner_token, "library.rescan", json!({})).await;
    assert_eq!(v["result"], json!({}));
    // A second rescan while one is pending is not an error.
    let v = call(&e.app, &e.owner_token, "library.rescan", json!({})).await;
    assert_eq!(v["result"], json!({}));
}

#[tokio::test]
async fn folder_list_and_media_get() {
    let e = env();
    let v = call(&e.app, &e.owner_token, "folder.list", json!({"path": ""})).await;
    assert_eq!(v["result"]["folders"], json!(["sub"]));
    let items = v["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["path"], "a.jpg");
    assert_eq!(items[0]["kind"], "image");
    assert!(items[0]["modified"].as_str().unwrap().ends_with('Z'));
    assert_eq!(v["result"]["next_cursor"], Value::Null);

    let v = call(
        &e.app,
        &e.owner_token,
        "folder.list",
        json!({"path": "sub", "limit": 1}),
    )
    .await;
    assert_eq!(v["result"]["items"][0]["path"], "sub/b.jpg");

    let id = item_id(&e, "a.jpg");
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["path"], "a.jpg");
    assert_eq!(v["result"]["rating"], Value::Null);
    assert_eq!(v["result"]["content_hash"], Value::Null);

    // A stored hash is shown only while it matches the record's size and mtime.
    let rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
    let mut h = StoredHash {
        hash: "abc".into(),
        scope: "full".into(),
        size: rec.size,
        mtime_ns: rec.mtime_ns,
    };
    e.cache.set_hash("a.jpg", Some(h.clone())).unwrap();
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["content_hash"], "abc");
    h.size += 1;
    let mut stale = rec.clone();
    stale.hash = Some(h);
    e.cache.put(&stale).unwrap();
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["content_hash"], Value::Null);

    let v = call(
        &e.app,
        &e.owner_token,
        "folder.list",
        json!({"path": "../x"}),
    )
    .await;
    assert_eq!(v["error"]["code"], codes::INVALID_PATH);
}

#[tokio::test]
async fn media_update_roundtrip() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    let v = call(
        &e.app,
        &e.owner_token,
        "media.update",
        json!({"id": id, "rating": 4, "subjects": ["cat"], "label": "x"}),
    )
    .await;
    assert_eq!(v["result"]["rating"], 4);
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["rating"], 4);
    assert_eq!(v["result"]["subjects"], json!(["cat"]));
    assert_eq!(v["result"]["label"], "x");
}

#[tokio::test]
async fn errors_map_to_codes() {
    let e = env();
    let t = &e.owner_token;
    let v = call(
        &e.app,
        t,
        "media.get",
        json!({"id": "00000000-0000-0000-0000-000000000000"}),
    )
    .await;
    assert_eq!(v["error"]["code"], codes::NOT_FOUND);
    let v = call(&e.app, t, "nope.nope", json!({})).await;
    assert_eq!(v["error"]["code"], codes::METHOD_NOT_FOUND);
    let v = call(&e.app, t, "media.get", json!({"id": 5})).await;
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS);
    let id = item_id(&e, "a.jpg");
    let v = call(&e.app, t, "media.update", json!({"id": id, "rating": 9})).await;
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS);

    let auth = format!("Bearer {t}");
    let (status, bytes) = post_raw(&e.app, Some(&auth), "{not json").await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], codes::PARSE_ERROR);
    assert_eq!(v["id"], 0);
    let (_, bytes) = post_raw(
        &e.app,
        Some(&auth),
        r#"{"jsonrpc":"1.0","id":3,"method":"server.info"}"#,
    )
    .await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_REQUEST);
    assert_eq!(v["id"], 3);
    let (_, bytes) = post_raw(&e.app, Some(&auth), r#"[1,2]"#).await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], codes::INVALID_REQUEST);

    // A broken sidecar makes the item read-only.
    fs::write(e.root.join("a.jpg.xmp"), "<not xml").unwrap();
    scan(&e.lib, &e.cache, &Gallery).unwrap();
    let v = call(&e.app, t, "media.get", json!({"id": id})).await;
    assert!(v["result"]["read_only_reason"].is_string());
    let v = call(&e.app, t, "media.update", json!({"id": id, "rating": 2})).await;
    assert_eq!(v["error"]["code"], codes::READ_ONLY);
}

#[tokio::test]
async fn ratings_are_per_user() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    call(
        &e.app,
        &e.owner_token,
        "media.update",
        json!({"id": id, "rating": 5}),
    )
    .await;
    call(
        &e.app,
        &e.guest_token,
        "media.update",
        json!({"id": id, "rating": 2}),
    )
    .await;
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["rating"], 5);
    let v = call(&e.app, &e.guest_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["rating"], 2);
    let v = call(&e.app, &e.guest_token, "folder.list", json!({"path": ""})).await;
    assert_eq!(v["result"]["items"][0]["rating"], 2);
    let _ = e.guest;
}

#[derive(Clone)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

#[tokio::test]
async fn token_not_logged() {
    let e = env();
    let buf = Buf(Arc::new(Mutex::new(Vec::new())));
    let sub = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(sub);
    let bad = "definitely-wrong-token-123";
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"server.info"}"#;
    post_raw(&e.app, Some(&format!("Bearer {bad}")), body).await;
    post_raw(&e.app, Some(&format!("Bearer {}", e.owner_token)), body).await;
    call(
        &e.app,
        &e.owner_token,
        "media.get",
        json!({"id": "missing"}),
    )
    .await;
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(!log.contains(bad));
    assert!(!log.contains(&e.owner_token));
    assert!(!log.contains(&token_hash(&e.owner_token)));
}
