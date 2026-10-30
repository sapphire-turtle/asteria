//! `organize.apply`: batches of moves and trashes, dry runs, request checks and the journal.

mod common;

use std::fs;

use asteria_framework::journal::{JournalEntry, OpKind};
use asteria_framework_api::codes;
use common::{Env, call, env, item_id};
use serde_json::{Value, json};

fn mv(e: &Env, from: &str, to: &str) -> Value {
    json!({"item_id": item_id(e, from), "kind": "move", "to": to})
}

fn trash(e: &Env, from: &str) -> Value {
    json!({"item_id": item_id(e, from), "kind": "trash"})
}

async fn apply(e: &Env, ops: Vec<Value>, dry_run: bool, label: Option<&str>) -> Value {
    call(
        &e.app,
        &e.owner_token,
        "organize.apply",
        json!({"ops": ops, "dry_run": dry_run, "label": label}),
    )
    .await
}

fn journal(e: &Env) -> Vec<JournalEntry> {
    e.lib.journal().unwrap().read_all().unwrap()
}

#[tokio::test]
async fn apply_moves_and_reports_new_ids() {
    let e = env();
    let v = apply(&e, vec![mv(&e, "a.jpg", "2025/x.jpg")], false, None).await;
    let r = &v["result"]["results"][0];
    assert_eq!(r["status"], "done");
    assert_eq!(r["path"], "a.jpg");
    assert_eq!(r["new_path"], "2025/x.jpg");
    assert!(v["result"]["batch_id"].is_string());
    let got = call(
        &e.app,
        &e.owner_token,
        "media.get",
        json!({"id": r["new_item_id"]}),
    )
    .await;
    assert_eq!(got["result"]["path"], "2025/x.jpg");
    let list = call(&e.app, &e.owner_token, "folder.list", json!({"path": ""})).await;
    assert!(
        list["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["path"] != "a.jpg")
    );
    assert!(e.root.join("2025/x.jpg").exists() && !e.root.join("a.jpg").exists());
}

#[tokio::test]
async fn trash_reports_no_new_id_and_hides_the_item() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    let v = apply(&e, vec![trash(&e, "a.jpg")], false, None).await;
    let r = &v["result"]["results"][0];
    assert_eq!(r["status"], "done");
    assert!(r.get("new_path").is_none() && r.get("new_item_id").is_none());
    assert!(!e.root.join("a.jpg").exists());
    let got = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(got["error"]["code"], codes::NOT_FOUND);
}

#[tokio::test]
async fn dry_run_changes_nothing() {
    let e = env();
    let n = journal(&e).len();
    let v = apply(&e, vec![mv(&e, "a.jpg", "x/y.jpg")], true, None).await;
    assert_eq!(v["result"]["results"][0]["status"], "done");
    assert_eq!(v["result"]["results"][0]["new_path"], "x/y.jpg");
    assert!(v["result"]["batch_id"].is_null());
    assert!(e.root.join("a.jpg").exists() && !e.root.join("x").exists());
    assert_eq!(journal(&e).len(), n);
}

#[tokio::test]
async fn dry_run_sees_earlier_ops() {
    let e = env();
    let ok = apply(
        &e,
        vec![mv(&e, "a.jpg", "c.jpg"), mv(&e, "sub/b.jpg", "a.jpg")],
        true,
        None,
    )
    .await;
    assert_eq!(ok["result"]["results"][0]["status"], "done");
    assert_eq!(ok["result"]["results"][1]["status"], "done");
    let bad = apply(
        &e,
        vec![mv(&e, "sub/b.jpg", "a.jpg"), mv(&e, "a.jpg", "c.jpg")],
        true,
        None,
    )
    .await;
    assert_eq!(bad["result"]["results"][0]["status"], "skipped");
    assert_eq!(bad["result"]["results"][0]["reason"], "target_exists");
    assert_eq!(bad["result"]["results"][1]["status"], "done");
}

#[tokio::test]
async fn one_bad_op_does_not_stop_the_batch() {
    let e = env();
    fs::write(e.root.join("taken.jpg"), "x").unwrap();
    let v = apply(
        &e,
        vec![mv(&e, "a.jpg", "taken.jpg"), mv(&e, "sub/b.jpg", "z.jpg")],
        false,
        None,
    )
    .await;
    let rs = &v["result"]["results"];
    assert_eq!(rs[0]["status"], "skipped");
    assert_eq!(rs[0]["reason"], "target_exists");
    assert_eq!(rs[1]["status"], "done");
    assert!(e.root.join("a.jpg").exists() && e.root.join("z.jpg").exists());
}

#[tokio::test]
async fn request_errors_change_nothing() {
    let e = env();
    let n = journal(&e).len();
    let a = item_id(&e, "a.jpg");
    let cases: Vec<(Vec<Value>, i32)> = vec![
        (
            vec![mv(&e, "a.jpg", "p.jpg"), mv(&e, "a.jpg", "q.jpg")],
            codes::INVALID_PARAMS,
        ),
        (vec![mv(&e, "a.jpg", "../x.jpg")], codes::INVALID_PATH),
        (vec![mv(&e, "a.jpg", "x.png")], codes::INVALID_PARAMS),
        (
            vec![mv(&e, "a.jpg", "same.jpg"), mv(&e, "sub/b.jpg", "same.jpg")],
            codes::INVALID_PARAMS,
        ),
    ];
    for (ops, code) in cases {
        let v = apply(&e, ops, false, None).await;
        assert_eq!(v["error"]["code"], code, "{v}");
    }
    // The extension may differ in ASCII case only.
    let ok = apply(&e, vec![mv(&e, "a.jpg", "A.JPG")], true, None).await;
    assert!(ok.get("error").is_none(), "{ok}");
    let label = "x".repeat(201);
    let v = apply(&e, vec![], false, Some(&label)).await;
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS);
    let v = apply(&e, vec![], false, Some("a\nb")).await;
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS);
    assert!(e.root.join("a.jpg").exists() && e.root.join("sub/b.jpg").exists());
    assert_eq!(journal(&e).len(), n);
    assert!(!a.is_empty());
}

#[tokio::test]
async fn unknown_item_is_missing() {
    let e = env();
    let v = apply(
        &e,
        vec![json!({"item_id": "nope", "kind": "trash"})],
        false,
        None,
    )
    .await;
    let r = &v["result"]["results"][0];
    assert_eq!(r["status"], "skipped");
    assert_eq!(r["reason"], "missing");
    assert_eq!(r["path"], "");
}

#[tokio::test]
async fn emptied_folders_are_removed_but_not_the_root() {
    let e = env();
    apply(&e, vec![mv(&e, "sub/b.jpg", "b2.jpg")], false, None).await;
    assert!(!e.root.join("sub").exists());
    assert!(e.root.exists() && e.root.join(".asteria").exists());
    apply(&e, vec![mv(&e, "a.jpg", "n/m/a.jpg")], false, None).await;
    apply(&e, vec![mv(&e, "n/m/a.jpg", "a.jpg")], false, None).await;
    assert!(!e.root.join("n").exists());
}

#[tokio::test]
async fn journal_has_header_ops_and_end() {
    let e = env();
    let before = journal(&e).len();
    let v = apply(
        &e,
        vec![mv(&e, "a.jpg", "c.jpg"), trash(&e, "sub/b.jpg")],
        false,
        Some("tidy"),
    )
    .await;
    let id = v["result"]["batch_id"].as_str().unwrap().to_string();
    let lines: Vec<JournalEntry> = journal(&e).into_iter().skip(before).collect();
    let kinds: Vec<OpKind> = lines.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        vec![
            OpKind::Batch,
            OpKind::Rename,
            OpKind::Trash,
            OpKind::BatchEnd
        ]
    );
    assert!(lines.iter().all(|l| l.plan_id.unwrap().0.to_string() == id));
    assert_eq!(lines[0].after["op_count"], 2);
    assert_eq!(lines[0].after["label"], "tidy");
    assert!(lines[0].after["undo_of"].is_null());
    assert_eq!(lines[3].after["done"], 2);
    assert_eq!(lines[3].after["skipped"], 0);
}

#[tokio::test]
async fn revision_moves_after_apply() {
    let e = env();
    let rev = |v: &Value| v["result"]["revision"].as_u64().unwrap();
    let a = call(&e.app, &e.owner_token, "library.info", json!({})).await;
    apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, None).await;
    let b = call(&e.app, &e.owner_token, "library.info", json!({})).await;
    assert!(rev(&b) > rev(&a));
}
