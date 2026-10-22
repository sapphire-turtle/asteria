#![allow(dead_code)]
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_server::{
    AppState, Cache, MediaFilter, ServerConfig, TokenEntry, Tokens, new_token, router, scan,
    spawn_writer, token_hash,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use grain_id::GrainId;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

pub struct Gallery;
impl MediaFilter for Gallery {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

pub struct Env {
    pub _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub lib: Arc<Library>,
    pub cache: Arc<Cache>,
    pub app: Router,
    pub owner_token: String,
    pub guest_token: String,
    pub _rescan_rx: tokio::sync::mpsc::Receiver<()>,
}

pub fn env() -> Env {
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
        guest_token,
        _rescan_rx: rescan_rx,
    }
}

pub async fn post_raw(app: &Router, token: Option<&str>, body: &str) -> (StatusCode, Vec<u8>) {
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

pub async fn call(app: &Router, token: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}).to_string();
    let (status, bytes) = post_raw(app, Some(&format!("Bearer {token}")), &body).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 7);
    v
}

pub fn item_id(e: &Env, path: &str) -> String {
    e.cache.get_by_path(path).unwrap().unwrap().id.0
}
