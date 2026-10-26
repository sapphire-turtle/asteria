mod common;

use std::fs;

use asteria_framework::hash::file_hash;
use asteria_framework_server::scan;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{Value, json};
use tower::ServiceExt;

const CHUNK: usize = 8 * 1024 * 1024;

async fn put(app: &Router, token: Option<&str>, uri: &str, body: Vec<u8>) -> StatusCode {
    let mut req = Request::put(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    app.clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

/// A JPEG padded to `len` bytes (trailing bytes are ignored by decoders).
fn big_jpeg(e: &Env, len: usize) -> Vec<u8> {
    let mut bytes = fs::read(e.root.join("a.jpg")).unwrap();
    bytes.resize(len, 7);
    bytes
}

async fn begin(e: &Env, token: &str, bytes: &[u8]) -> String {
    let tmp = e._dir.path().join("hash-me");
    fs::write(&tmp, bytes).unwrap();
    let v = call(
        &e.app,
        token,
        "upload.begin",
        json!({"size": bytes.len(), "file_hash": file_hash(&tmp).unwrap().to_string()}),
    )
    .await;
    assert_eq!(v["result"]["chunk_size"], CHUNK);
    v["result"]["session"].as_str().unwrap().to_string()
}

fn code(v: &Value) -> i64 {
    v["error"]["code"].as_i64().unwrap()
}

#[tokio::test]
async fn full_upload_over_http() {
    let e = env();
    let bytes = big_jpeg(&e, 2 * CHUNK + 1000);
    let session = begin(&e, &e.owner_token, &bytes).await;
    let tok = Some(e.owner_token.as_str());
    for (i, part) in bytes.chunks(CHUNK).enumerate() {
        let s = put(
            &e.app,
            tok,
            &format!("/upload/{session}/{i}"),
            part.to_vec(),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }
    let v = call(
        &e.app,
        &e.owner_token,
        "upload.status",
        json!({"session": session}),
    )
    .await;
    assert_eq!(v["result"]["missing"], json!([]));

    let v = call(
        &e.app,
        &e.owner_token,
        "upload.commit",
        json!({"files": [{"session": session, "path": "trip/x.jpg"}], "allow_duplicate": false}),
    )
    .await;
    assert_eq!(v["result"]["outcome"], "placed", "{v}");
    assert_eq!(v["result"]["files"][0]["path"], "Inbox/trip/x.jpg");
    assert_eq!(fs::read(e.root.join("Inbox/trip/x.jpg")).unwrap(), bytes);
    // A rescan was requested.
    let mut e = e;
    assert!(e._rescan_rx.try_recv().is_ok());

    scan(&e.lib, &e.cache, &Photos).unwrap();
    let v = call(
        &e.app,
        &e.owner_token,
        "folder.list",
        json!({"path": "Inbox/trip"}),
    )
    .await;
    assert_eq!(v["result"]["items"][0]["path"], "Inbox/trip/x.jpg", "{v}");
}

#[tokio::test]
async fn chunk_over_limit_is_413() {
    let e = env();
    let bytes = big_jpeg(&e, 2 * CHUNK);
    let session = begin(&e, &e.owner_token, &bytes).await;
    let s = put(
        &e.app,
        Some(&e.owner_token),
        &format!("/upload/{session}/0"),
        vec![0; CHUNK + 1],
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn chunk_without_token_is_401() {
    let e = env();
    let s = put(&e.app, None, "/upload/x/0", vec![1]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn other_users_session_is_404() {
    let e = env();
    let bytes = big_jpeg(&e, 1000);
    let session = begin(&e, &e.owner_token, &bytes).await;
    let uri = format!("/upload/{session}/0");
    assert_eq!(
        put(&e.app, Some(&e.guest_token), &uri, bytes.clone()).await,
        StatusCode::NOT_FOUND
    );
    for method in ["upload.status", "upload.abort"] {
        let v = call(&e.app, &e.guest_token, method, json!({"session": session})).await;
        assert_eq!(code(&v), -32001, "{method}: {v}");
    }
    // Still the owner's, untouched.
    assert_eq!(
        put(&e.app, Some(&e.owner_token), &uri, bytes).await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn chunk_errors_map_to_statuses() {
    let e = env();
    let bytes = big_jpeg(&e, 1000);
    let session = begin(&e, &e.owner_token, &bytes).await;
    let tok = Some(e.owner_token.as_str());
    // Wrong length and out-of-range index are both Invalid.
    assert_eq!(
        put(&e.app, tok, &format!("/upload/{session}/0"), vec![1; 5]).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        put(&e.app, tok, &format!("/upload/{session}/9"), bytes).await,
        StatusCode::BAD_REQUEST
    );
    let v = call(
        &e.app,
        &e.owner_token,
        "upload.commit",
        json!({"files": [{"session": session, "path": "x.jpg"}], "allow_duplicate": false}),
    )
    .await;
    assert_eq!(code(&v), -32007, "{v}");
}

#[tokio::test]
async fn rpc_limit_still_64k() {
    let e = env();
    let pad = "x".repeat(65 * 1024);
    let body = json!({"jsonrpc":"2.0","id":1,"method":"server.info","params":{"pad":pad}});
    let (s, _) = post_raw(
        &e.app,
        Some(&format!("Bearer {}", e.owner_token)),
        &body.to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn check_reports_existing_paths() {
    let e = env();
    hash_all(&e);
    let h = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash.unwrap();
    let v = call(
        &e.app,
        &e.owner_token,
        "upload.check",
        json!({"hashes": [
            {"hash": h.hash, "scope": h.scope},
            {"hash": format!("blake3:{}", "0".repeat(64)), "scope": h.scope},
        ]}),
    )
    .await;
    let existing = v["result"]["existing"].as_array().unwrap();
    assert_eq!(existing.len(), 1, "{v}");
    assert_eq!(existing[0]["hash"], h.hash);
    assert!(
        existing[0]["paths"]
            .as_array()
            .unwrap()
            .contains(&json!("a.jpg"))
    );

    let many: Vec<Value> = (0..1001)
        .map(|_| json!({"hash": "blake3:00", "scope": "content"}))
        .collect();
    let v = call(
        &e.app,
        &e.owner_token,
        "upload.check",
        json!({"hashes": many}),
    )
    .await;
    assert_eq!(code(&v), -32602, "{v}");
}

#[tokio::test]
async fn begin_rejects_a_malformed_hash_and_abort_removes() {
    let e = env();
    let v = call(
        &e.app,
        &e.owner_token,
        "upload.begin",
        json!({"size": 10, "file_hash": "nope"}),
    )
    .await;
    assert_eq!(code(&v), -32602, "{v}");
    let session = begin(&e, &e.owner_token, &big_jpeg(&e, 1000)).await;
    let v = call(
        &e.app,
        &e.owner_token,
        "upload.abort",
        json!({"session": session}),
    )
    .await;
    assert!(v.get("error").is_none(), "{v}");
    let v = call(
        &e.app,
        &e.owner_token,
        "upload.status",
        json!({"session": session}),
    )
    .await;
    assert_eq!(code(&v), -32001);
}
