mod common;

use std::fs;

use asteria_framework_api::codes;
use asteria_framework_server::{StoredHash, scan};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{Value, json};
use tower::ServiceExt;

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
    ] {
        let (status, bytes) = post_raw(&e.app, header, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{header:?}");
        assert!(bytes.is_empty());
    }
    let (status, _) = post_raw(&e.app, Some(&good), body).await;
    assert_eq!(status, StatusCode::OK);
    // The scheme is case-insensitive.
    for scheme in ["bearer", "BEARER"] {
        let h = format!("{scheme} {}", e.owner_token);
        let (status, _) = post_raw(&e.app, Some(&h), body).await;
        assert_eq!(status, StatusCode::OK, "{scheme}");
    }
    let (status, _) = post_raw(&e.app, Some("bearer wrong"), body).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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
    assert_eq!(v["result"]["app"], "asteria-photos");
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
    assert_eq!(v["result"]["file_hash"], Value::Null);

    // A stored hash is shown only while it matches the record's size and mtime.
    let rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
    let mut h = StoredHash {
        hash: "abc".into(),
        scope: "full".into(),
        file_hash: "def".into(),
        size: rec.size,
        mtime_ns: rec.mtime_ns,
    };
    e.cache.set_hash("a.jpg", Some(h.clone())).unwrap();
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["content_hash"], "abc");
    assert_eq!(v["result"]["file_hash"], "def");
    h.size += 1;
    let mut stale = rec.clone();
    stale.hash = Some(h);
    e.cache.put(&stale).unwrap();
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(v["result"]["content_hash"], Value::Null);
    assert_eq!(v["result"]["file_hash"], Value::Null);

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
async fn library_info_reports_a_revision_that_moves_after_a_write() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    let v = call(&e.app, &e.owner_token, "library.info", json!({})).await;
    let before = v["result"]["revision"].as_u64().unwrap();
    let v = call(
        &e.app,
        &e.owner_token,
        "media.update",
        json!({"id": id, "rating": 4}),
    )
    .await;
    assert_eq!(v["result"]["rating"], 4);
    let v = call(&e.app, &e.owner_token, "library.info", json!({})).await;
    assert!(v["result"]["revision"].as_u64().unwrap() > before);
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
    scan(&e.lib, &e.cache, &Photos).unwrap();
    let v = call(&e.app, t, "media.get", json!({"id": id})).await;
    assert!(v["result"]["read_only_reason"].is_string());
    let v = call(&e.app, t, "media.update", json!({"id": id, "rating": 2})).await;
    assert_eq!(v["error"]["code"], codes::READ_ONLY);
    let root = e.root.to_str().unwrap();
    assert!(!v["error"]["message"].as_str().unwrap().contains(root));
    let v = call(&e.app, t, "media.get", json!({"id": id})).await;
    assert!(
        !v["result"]["read_only_reason"]
            .as_str()
            .unwrap()
            .contains(root)
    );
}

#[tokio::test]
async fn busy_message_has_no_path() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    // A temp file of this process's in-flight write to the sidecar.
    let tmp = asteria_framework::fsutil::tmp_path_for(&e.root.join("a.jpg.xmp"));
    fs::write(tmp, b"x").unwrap();
    let v = call(
        &e.app,
        &e.owner_token,
        "media.update",
        json!({"id": id, "rating": 2}),
    )
    .await;
    assert_eq!(v["error"]["code"], codes::BUSY);
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        !msg.contains(e.root.to_str().unwrap()) && !msg.contains('/'),
        "{msg}"
    );
}

#[tokio::test]
async fn oversized_body_is_413() {
    let e = env();
    let big = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"server.info","params":{{"x":"{}"}}}}"#,
        "a".repeat(70 * 1024)
    );
    let (status, _) = post_raw(&e.app, Some(&format!("Bearer {}", e.owner_token)), &big).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn params_may_be_omitted() {
    let e = env();
    let body = r#"{"jsonrpc":"2.0","id":4,"method":"server.info"}"#;
    let (status, bytes) = post_raw(&e.app, Some(&format!("Bearer {}", e.owner_token)), body).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["id"], 4);
    assert_eq!(v["result"]["app"], "asteria-photos");
}

fn bulk(e: &Env, n: usize) {
    let recs: Vec<_> = (0..n)
        .map(|i| {
            let mut r = e.cache.get_by_path("a.jpg").unwrap().unwrap();
            r.path = format!("bulk/f{i:05}.jpg");
            r.id = asteria_framework_server::item_id_for(&e.lib.config().id, &r.path);
            r
        })
        .collect();
    e.cache.put_many(&recs).unwrap();
}

#[tokio::test]
async fn folder_list_limits_and_paging() {
    let e = env();
    bulk(&e, 1001);
    let t = &e.owner_token;
    // Default limit is 200.
    let v = call(&e.app, t, "folder.list", json!({"path": "bulk"})).await;
    assert_eq!(v["result"]["items"].as_array().unwrap().len(), 200);
    assert!(v["result"]["next_cursor"].is_string());
    // Requested limit is capped at 1000.
    let v = call(
        &e.app,
        t,
        "folder.list",
        json!({"path": "bulk", "limit": 5000}),
    )
    .await;
    assert_eq!(v["result"]["items"].as_array().unwrap().len(), 1000);
    assert!(v["result"]["next_cursor"].is_string());
    // Paging: two pages cover everything without overlap.
    let v1 = call(
        &e.app,
        t,
        "folder.list",
        json!({"path": "bulk", "limit": 600}),
    )
    .await;
    let cur = v1["result"]["next_cursor"].clone();
    assert!(cur.is_string());
    let v2 = call(
        &e.app,
        t,
        "folder.list",
        json!({"path": "bulk", "limit": 600, "cursor": cur}),
    )
    .await;
    assert_eq!(v2["result"]["items"].as_array().unwrap().len(), 401);
    assert_eq!(v2["result"]["next_cursor"], Value::Null);
    assert_eq!(v2["result"]["items"][0]["path"], "bulk/f00600.jpg");
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
}
