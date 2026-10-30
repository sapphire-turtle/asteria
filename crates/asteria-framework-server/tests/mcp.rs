//! `/mcp`: the tools over a real TCP connection, with rmcp's client and the owner's token.

mod common;

use asteria_framework::journal::{JournalEntry, OpKind};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use common::*;
use http_body_util::BodyExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const INSTRUCTIONS_START: &str = "Asteria manages a self-hosted media library. Browse with \
folder_list and media_get; look at pictures with media_preview.";

const FRAMEWORK_TOOLS: [&str; 9] = [
    "library_info",
    "folder_list",
    "media_get",
    "media_update",
    "organize_apply",
    "organize_status",
    "organize_undo",
    "organize_history",
    "media_preview",
];

async fn tool(client: &RunningService<RoleClient, ()>, name: &str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args))
        .await
        .unwrap()
}

fn text(r: &CallToolResult) -> &str {
    assert_eq!(r.content.len(), 1);
    &r.content[0].as_text().unwrap().text
}

fn ok_json(r: &CallToolResult) -> Value {
    assert_ne!(r.is_error, Some(true), "tool error: {}", text(r));
    serde_json::from_str(text(r)).unwrap()
}

fn journal(e: &Env) -> Vec<JournalEntry> {
    e.lib.journal().unwrap().read_all().unwrap()
}

fn actor(entry: &JournalEntry) -> Value {
    serde_json::to_value(entry.actor).unwrap()
}

fn agent(e: &Env) -> Value {
    json!({"agent": e.lib.owner().to_string()})
}

#[tokio::test]
async fn lists_the_framework_tools() {
    let e = env();
    let client = connect(&e).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for name in FRAMEWORK_TOOLS {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }
    assert_eq!(names.len(), 9, "{names:?}");
    let info = client.peer_info().unwrap();
    let instructions = info.instructions.as_deref().unwrap();
    assert!(
        instructions.starts_with(INSTRUCTIONS_START),
        "{instructions}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn media_update_is_recorded_as_the_agent() {
    let e = env();
    let client = connect(&e).await;
    let id = item_id(&e, "a.jpg");
    let r = tool(&client, "media_update", json!({"id": id, "rating": 4})).await;
    assert_eq!(ok_json(&r)["rating"], 4);
    let got = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(got["result"]["rating"], 4);
    let last_write = |e: &Env| {
        journal(e)
            .into_iter()
            .rev()
            .find(|l| l.kind == OpKind::WriteSidecar)
            .unwrap()
    };
    assert_eq!(actor(&last_write(&e)), agent(&e));

    call(
        &e.app,
        &e.owner_token,
        "media.update",
        json!({"id": id, "rating": 4}),
    )
    .await;
    assert_eq!(
        actor(&last_write(&e)),
        json!({"user": e.lib.owner().to_string()})
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn organize_apply_is_recorded_as_the_agent() {
    let e = env();
    let client = connect(&e).await;
    let before = journal(&e).len();
    let ops = json!([{"item_id": item_id(&e, "a.jpg"), "kind": "move", "to": "2025/a.jpg"}]);
    let r = tool(
        &client,
        "organize_apply",
        json!({"ops": ops, "label": "by year"}),
    )
    .await;
    let v = ok_json(&r);
    assert_eq!(v["results"][0]["status"], "done");
    assert!(e.root.join("2025/a.jpg").exists());
    let lines: Vec<JournalEntry> = journal(&e).into_iter().skip(before).collect();
    let header = lines.iter().find(|l| l.kind == OpKind::Batch).unwrap();
    assert_eq!(actor(header), agent(&e));
    let op = lines.iter().find(|l| l.kind == OpKind::Move).unwrap();
    assert_eq!(actor(op), agent(&e));

    let h = ok_json(&tool(&client, "organize_history", json!({})).await);
    assert_eq!(h["batches"][0]["batch_id"], v["batch_id"]);
    assert_eq!(h["batches"][0]["agent"], true);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn organize_undo_is_recorded_as_the_agent() {
    let e = env();
    let client = connect(&e).await;
    let ops = json!([{"item_id": item_id(&e, "a.jpg"), "kind": "move", "to": "2025/a.jpg"}]);
    let v = ok_json(&tool(&client, "organize_apply", json!({"ops": ops})).await);
    let before = journal(&e).len();
    let u = ok_json(&tool(&client, "organize_undo", json!({"batch_id": v["batch_id"]})).await);
    assert_eq!(u["results"][0]["status"], "done");
    assert!(e.root.join("a.jpg").exists());
    let lines: Vec<JournalEntry> = journal(&e).into_iter().skip(before).collect();
    let header = lines.iter().find(|l| l.kind == OpKind::Batch).unwrap();
    assert_eq!(header.after["undo_of"], v["batch_id"]);
    let ops: Vec<&JournalEntry> = lines
        .iter()
        .filter(|l| !matches!(l.kind, OpKind::Batch | OpKind::BatchEnd))
        .collect();
    assert!(!ops.is_empty());
    for l in std::iter::once(header).chain(ops) {
        assert_eq!(actor(l), agent(&e), "{:?}", l.kind);
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn rpc_errors_are_tool_errors() {
    let e = env();
    let client = connect(&e).await;
    let r = tool(&client, "media_get", json!({"id": "nope"})).await;
    assert_eq!(r.is_error, Some(true));
    let rpc = call(&e.app, &e.owner_token, "media.get", json!({"id": "nope"})).await;
    assert_eq!(text(&r), rpc["error"]["message"].as_str().unwrap());
    assert_eq!(text(&r), "not found");

    let ops = json!([{"item_id": item_id(&e, "a.jpg"), "kind": "move", "to": "c.jpg"}]);
    let v = ok_json(&tool(&client, "organize_apply", json!({"ops": ops})).await);
    let batch = json!({"batch_id": v["batch_id"]});
    ok_json(&tool(&client, "organize_undo", batch.clone()).await);
    let again = tool(&client, "organize_undo", batch).await;
    assert_eq!(again.is_error, Some(true));
    assert!(text(&again).contains("already undone"), "{}", text(&again));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error() {
    let e = env();
    let client = connect(&e).await;
    let r = client
        .call_tool(CallToolRequestParams::new("no_such_tool"))
        .await;
    match r {
        Err(rmcp::ServiceError::McpError(err)) => {
            assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn mcp_needs_a_token() {
    let e = env();
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "t", "version": "1"}}
    })
    .to_string();
    for auth in [None, Some("Bearer definitely-wrong-token")] {
        let mut req = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let resp = e
            .app
            .clone()
            .oneshot(req.body(Body::from(init.clone())).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty());
    }
}

fn preview_text(r: &CallToolResult, i: usize) -> &str {
    &r.content[i].as_text().unwrap().text
}

#[tokio::test]
async fn media_preview_returns_images() {
    if !have_tool("vipsthumbnail") {
        return;
    }
    let e = env();
    hash_all(&e);
    let client = connect(&e).await;
    let id = item_id(&e, "a.jpg");
    let r = tool(&client, "media_preview", json!({"ids": [id]})).await;
    assert_ne!(r.is_error, Some(true));
    assert_eq!(r.content.len(), 2);
    assert_eq!(preview_text(&r, 0), format!("{id}:"));
    let img = r.content[1].as_image().unwrap();
    assert_eq!(img.mime_type, "image/webp");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&img.data)
        .unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn media_preview_reports_each_missing_item() {
    if !have_tool("vipsthumbnail") {
        return;
    }
    let e = env();
    hash_all(&e);
    let client = connect(&e).await;
    let id = item_id(&e, "a.jpg");
    let r = tool(&client, "media_preview", json!({"ids": ["nope", id]})).await;
    assert_ne!(r.is_error, Some(true));
    assert_eq!(r.content.len(), 3);
    assert_eq!(preview_text(&r, 0), "nope: not found");
    assert_eq!(preview_text(&r, 1), format!("{id}:"));
    assert!(r.content[2].as_image().is_some());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn media_preview_takes_at_most_four() {
    let e = env();
    let client = connect(&e).await;
    let r = tool(
        &client,
        "media_preview",
        json!({"ids": ["1", "2", "3", "4", "5"]}),
    )
    .await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(text(&r), "at most 4 items per preview");
    let r = tool(&client, "media_preview", json!({"ids": []})).await;
    assert_eq!(r.is_error, Some(true));
    assert_eq!(text(&r), "at least one item");
    client.cancel().await.unwrap();
}
