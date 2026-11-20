mod common;

use std::fs;

use asteria_common_api::codes;
use asteria_common_server::scan;
use common::*;
use serde_json::{Value, json};

fn op(id: &str, item: &str, value: i8, at: &str) -> Value {
    json!({"id": id, "kind": "rating", "item": item, "value": value, "at": at})
}

async fn apply(e: &Env, ops: Vec<Value>) -> Value {
    call(&e.app, &e.owner_token, "ops.apply", json!({ "ops": ops })).await
}

fn status(v: &Value, i: usize) -> &str {
    v["result"]["results"][i]["status"].as_str().unwrap()
}

async fn rating(e: &Env, id: &str) -> Value {
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    v["result"]["rating"].clone()
}

#[tokio::test]
async fn applies_and_reports_the_item() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let v = apply(&e, vec![op("o1", &a, 4, "2026-11-01T00:00:00Z")]).await;
    let r = &v["result"]["results"][0];
    assert_eq!(
        (r["id"].as_str(), r["status"].as_str()),
        (Some("o1"), Some("applied"))
    );
    assert_eq!(r["item"]["rating"], 4);
    assert!(r.get("reason").is_none());
    assert_eq!(rating(&e, &a).await, 4);
}

#[tokio::test]
async fn an_older_rating_is_superseded() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let v = apply(&e, vec![op("o1", &a, 5, "2026-11-01T10:00:00Z")]).await;
    assert_eq!(status(&v, 0), "applied");
    let v = apply(&e, vec![op("o2", &a, 2, "2026-11-01T09:00:00Z")]).await;
    assert_eq!(status(&v, 0), "superseded");
    assert_eq!(v["result"]["results"][0]["item"]["rating"], 5);
    assert_eq!(rating(&e, &a).await, 5);
}

#[tokio::test]
async fn same_time_takes_the_greater_value() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let t = "2026-11-01T10:00:00Z";
    let v = apply(&e, vec![op("o1", &a, 2, t)]).await;
    assert_eq!(status(&v, 0), "applied");
    let v = apply(&e, vec![op("o2", &a, 4, t)]).await;
    assert_eq!(status(&v, 0), "applied");
    assert_eq!(rating(&e, &a).await, 4);
    let v = apply(&e, vec![op("o3", &a, 3, t)]).await;
    assert_eq!(status(&v, 0), "superseded");
    assert_eq!(rating(&e, &a).await, 4);
}

#[tokio::test]
async fn sending_the_same_op_twice_changes_nothing() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let o = op("o1", &a, 3, "2026-11-01T10:00:00Z");
    let v = apply(&e, vec![o.clone()]).await;
    assert_eq!(status(&v, 0), "applied");
    let v = apply(&e, vec![o]).await;
    assert_eq!(status(&v, 0), "superseded");
    assert_eq!(rating(&e, &a).await, 3);
}

#[tokio::test]
async fn a_future_time_is_clamped_to_now() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let v = apply(&e, vec![op("o1", &a, 5, "2999-01-01T00:00:00Z")]).await;
    assert_eq!(status(&v, 0), "applied");
    assert_eq!(rating(&e, &a).await, 5);
    // Sleep so that the server's time moves past the clamped one.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let v = call(
        &e.app,
        &e.owner_token,
        "media.update",
        json!({"id": a, "rating": 1}),
    )
    .await;
    assert_eq!(v["result"]["rating"], 1);
    assert_eq!(rating(&e, &a).await, 1);
}

#[tokio::test]
async fn refusals_are_per_op() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let t = "2026-11-01T10:00:00Z";
    let v = apply(
        &e,
        vec![
            op("missing", "00000000-0000-0000-0000-000000000000", 3, t),
            op("range", &a, 9, t),
            op("time", &a, 3, "yesterday"),
            op("good", &a, 3, t),
        ],
    )
    .await;
    let rs = v["result"]["results"].as_array().unwrap();
    let ids: Vec<_> = rs.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["missing", "range", "time", "good"]);
    let got: Vec<_> = rs
        .iter()
        .map(|r| (r["status"].as_str(), r["reason"].as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (Some("rejected"), Some("not_found")),
            (Some("rejected"), Some("invalid")),
            (Some("rejected"), Some("invalid")),
            (Some("applied"), None),
        ]
    );
    assert!(rs[0].get("item").is_none());
    assert_eq!(rating(&e, &a).await, 3);

    // A broken sidecar makes the item read-only.
    fs::write(e.root.join("a.jpg.xmp"), "<not xml").unwrap();
    scan(&e.lib, &e.cache, &Photos).unwrap();
    let v = apply(&e, vec![op("ro", &a, 4, "2026-11-02T00:00:00Z")]).await;
    let r = &v["result"]["results"][0];
    assert_eq!(
        (r["status"].as_str(), r["reason"].as_str()),
        (Some("rejected"), Some("read_only"))
    );
}

#[tokio::test]
async fn more_than_max_ops_is_invalid_params() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let t = "2026-11-01T10:00:00Z";
    let ops: Vec<Value> = (0..201).map(|i| op(&format!("o{i}"), &a, 3, t)).collect();
    let v = apply(&e, ops).await;
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS);
    let ops: Vec<Value> = (0..200).map(|i| op(&format!("o{i}"), &a, 3, t)).collect();
    let v = apply(&e, ops).await;
    assert_eq!(v["result"]["results"].as_array().unwrap().len(), 200);
}

#[tokio::test]
async fn a_sidecar_that_cannot_be_written_rejects_only_its_op() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let b = item_id(&e, "sub/b.jpg");
    let t = "2026-11-01T10:00:00Z";
    // Broken after the scan: the cache still thinks the item is writable.
    fs::write(e.root.join("a.jpg.xmp"), "<not xml").unwrap();
    let v = apply(&e, vec![op("broken", &a, 4, t), op("good", &b, 3, t)]).await;
    let rs = v["result"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"));
    let got: Vec<_> = rs
        .iter()
        .map(|r| (r["id"].as_str(), r["status"].as_str(), r["reason"].as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (Some("broken"), Some("rejected"), Some("read_only")),
            (Some("good"), Some("applied"), None),
        ]
    );
    assert_eq!(rating(&e, &b).await, 3);

    // An I/O error writing it (the sidecar's place is taken by a directory) does the same.
    fs::remove_file(e.root.join("a.jpg.xmp")).unwrap();
    scan(&e.lib, &e.cache, &Photos).unwrap();
    fs::create_dir(e.root.join("a.jpg.xmp")).unwrap();
    let v = apply(
        &e,
        vec![
            op("io", &a, 2, "2026-11-02T00:00:00Z"),
            op("good2", &b, 5, "2026-11-02T00:00:00Z"),
        ],
    )
    .await;
    let rs = v["result"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"));
    let got: Vec<_> = rs
        .iter()
        .map(|r| (r["id"].as_str(), r["status"].as_str(), r["reason"].as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (Some("io"), Some("rejected"), Some("read_only")),
            (Some("good2"), Some("applied"), None),
        ]
    );
    assert_eq!(rating(&e, &b).await, 5);
}

/// Applied, superseded and the rejections keep the values they had on the wire, and the
/// ratings that reached the Journal are one batch of `rate`s from the caller's key.
#[tokio::test]
async fn ops_apply_results_keep_their_wire_values() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let b = item_id(&e, "sub/b.jpg");
    let v = apply(&e, vec![op("o0", &a, 5, "2026-11-01T10:00:00Z")]).await;
    assert_eq!(status(&v, 0), "applied");
    // Broken after the scan: only writing it finds out.
    fs::write(e.root.join("sub/b.jpg.xmp"), "<not xml").unwrap();
    let v = apply(
        &e,
        vec![
            op("applied", &a, 2, "2026-11-02T10:00:00Z"),
            op("superseded", &a, 4, "2026-11-01T09:00:00Z"),
            op(
                "missing",
                "00000000-0000-0000-0000-000000000000",
                3,
                "2026-11-01T10:00:00Z",
            ),
            op("read_only", &b, 3, "2026-11-01T10:00:00Z"),
        ],
    )
    .await;
    let rs = v["result"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"));
    let got: Vec<_> = rs
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap(),
                r["status"].as_str().unwrap(),
                r.get("reason").and_then(Value::as_str),
                r.get("item").map(|i| i["rating"].clone()),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("applied", "applied", None, Some(json!(2))),
            ("superseded", "superseded", None, Some(json!(2))),
            ("missing", "rejected", Some("not_found"), None),
            ("read_only", "rejected", Some("read_only"), None),
        ]
    );

    let newest = e.state.journal().history(None, 1).unwrap();
    let batch = e.state.journal().status(newest[0].begin.batch_id).unwrap();
    assert_eq!(batch.summary.begin.label.as_deref(), Some("Rate 3 items"));
    assert!(
        batch.summary.begin.origin.as_str().starts_with("key:"),
        "{:?}",
        batch.summary.begin.origin
    );
    let kinds: Vec<&str> = batch.ops.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(kinds, ["rate", "rate", "rate"]);
}
