//! `organize.apply`: batches of moves and trashes, dry runs, request checks and the journal.

mod common;

use std::fs;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use asteria_common_api::{MAX_OPS, codes};
use asteria_common_core::MediaKind;
use asteria_common_server::{AppExtension, ExtContext, Rewrite};
use common::{Env, call, env, item_id};
use corundum_framework::journal::{BatchId, BatchRecords, BatchSummary};
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

/// Every batch in the journal, newest first.
fn journal(e: &Env) -> Vec<BatchSummary> {
    e.state.journal().history(None, 10_000).unwrap()
}

fn records(e: &Env, id: &str) -> BatchRecords {
    let id = BatchId(id.parse().unwrap());
    e.state.journal().status(id).unwrap()
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
    let cases: Vec<(Vec<Value>, i32, &str)> = vec![
        (
            vec![mv(&e, "a.jpg", "p.jpg"), mv(&e, "a.jpg", "q.jpg")],
            codes::INVALID_PARAMS,
            "item 1 appears twice",
        ),
        (vec![mv(&e, "a.jpg", "../x.jpg")], codes::INVALID_PATH, ""),
        (
            vec![mv(&e, "sub/b.jpg", "ok.jpg"), mv(&e, "a.jpg", "x.png")],
            codes::INVALID_PARAMS,
            "operation 1 changes the extension",
        ),
        (
            vec![mv(&e, "a.jpg", "same.jpg"), mv(&e, "sub/b.jpg", "same.jpg")],
            codes::INVALID_PARAMS,
            "operations 0 and 1 have the same target",
        ),
        // The same file on a case-insensitive filesystem (#33).
        (
            vec![mv(&e, "a.jpg", "same.jpg"), mv(&e, "sub/b.jpg", "SAME.jpg")],
            codes::INVALID_PARAMS,
            "operations 0 and 1 have the same target",
        ),
    ];
    for (ops, code, message) in cases {
        let v = apply(&e, ops, false, None).await;
        assert_eq!(v["error"]["code"], code, "{v}");
        if !message.is_empty() {
            assert_eq!(v["error"]["message"], message, "{v}");
        }
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
async fn the_journal_records_the_batch() {
    let e = env();
    let v = apply(
        &e,
        vec![mv(&e, "a.jpg", "c.jpg"), trash(&e, "sub/b.jpg")],
        false,
        Some("tidy"),
    )
    .await;
    let id = v["result"]["batch_id"].as_str().unwrap().to_string();
    let b = records(&e, &id);
    let kinds: Vec<&str> = b
        .ops
        .iter()
        .filter(|o| o.parent.is_none())
        .map(|o| o.kind.as_str())
        .collect();
    assert_eq!(kinds, ["move", "trash", "organize_tidy"]);
    assert_eq!(b.summary.begin.label.as_deref(), Some("tidy"));
    assert_eq!(b.summary.begin.undo_of, None);
    assert!(b.summary.complete && !b.summary.interrupted);
    assert_eq!(b.summary.skipped, 0);
    let origin = b.summary.begin.origin.as_str();
    assert!(origin.starts_with("key:"), "{origin}");
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

fn unknown_ops(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| json!({"item_id": format!("unknown-{i:08}-aaaaaaaa-bbbbbbbb-cccccccc"), "kind": "move", "to": format!("folder/sub{i}/file{i}.jpg")}))
        .collect()
}

#[tokio::test]
async fn max_ops_fit_in_a_request_and_one_more_is_refused() {
    let e = env();
    let v = apply(&e, unknown_ops(MAX_OPS), true, None).await;
    let rs = v["result"]["results"].as_array().unwrap();
    assert_eq!(rs.len(), MAX_OPS);
    assert!(
        rs.iter()
            .all(|r| r["status"] == "skipped" && r["reason"] == "missing")
    );
    let v = apply(&e, unknown_ops(MAX_OPS + 1), true, None).await;
    assert_eq!(v["error"]["code"], codes::INVALID_PARAMS);
    assert_eq!(v["error"]["message"], "at most 10000 operations per batch");
}

#[tokio::test]
async fn a_folder_that_still_holds_a_file_stays() {
    let e = env();
    fs::write(e.root.join("sub/other.jpg"), "x").unwrap();
    apply(&e, vec![mv(&e, "sub/b.jpg", "b2.jpg")], false, None).await;
    assert!(e.root.join("sub/other.jpg").exists());
}

// ---- organize.undo and organize.history ----

async fn undo(e: &Env, batch_id: &str, dry_run: bool) -> Value {
    call(
        &e.app,
        &e.owner_token,
        "organize.undo",
        json!({"batch_id": batch_id, "dry_run": dry_run}),
    )
    .await
}

fn batch_id(v: &Value) -> String {
    v["result"]["batch_id"].as_str().unwrap().to_string()
}

fn status(v: &Value, i: usize) -> &Value {
    &v["result"]["results"][i]["status"]
}

#[tokio::test]
async fn undo_puts_moves_back() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    let b = batch_id(&apply(&e, vec![mv(&e, "a.jpg", "2025/x.jpg")], false, None).await);
    assert!(e.root.join("2025/x.jpg").exists());
    let v = undo(&e, &b, false).await;
    assert_eq!(status(&v, 0), "done");
    assert_eq!(v["result"]["results"][0]["new_path"], "a.jpg");
    assert!(e.root.join("a.jpg").exists());
    assert!(!e.root.join("2025").exists());
    let got = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(got["result"]["path"], "a.jpg");
}

#[tokio::test]
async fn undo_restores_trash_with_its_rating() {
    let e = env();
    common::hash_all(&e);
    let id = item_id(&e, "a.jpg");
    let set = call(
        &e.app,
        &e.owner_token,
        "media.update",
        json!({"id": id, "rating": 4}),
    )
    .await;
    assert!(set.get("error").is_none(), "{set}");
    let hash =
        call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await["result"]["file_hash"]
            .clone();
    assert!(hash.is_string());
    let b = batch_id(&apply(&e, vec![trash(&e, "a.jpg")], false, None).await);
    // The trash is internal: a restore reports the place the item goes back to.
    let dry = undo(&e, &b, true).await;
    assert_eq!(dry["result"]["results"][0]["path"], "a.jpg");
    assert_eq!(dry["result"]["results"][0]["new_path"], "a.jpg");
    let v = undo(&e, &b, false).await;
    assert_eq!(status(&v, 0), "done");
    let r = &v["result"]["results"][0];
    assert_eq!(r["path"], "a.jpg");
    assert_eq!(r["new_path"], "a.jpg");
    assert_eq!(r["new_item_id"], id);
    assert!(e.root.join("a.jpg").exists());
    let got = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(got["result"]["rating"], 4);
    assert_eq!(got["result"]["file_hash"], hash);
    let trash_dir = e.root.join(".asteria/trash");
    assert!(trash_dir.is_dir());
    assert!(fs::read_dir(&trash_dir).unwrap().next().is_none());
}

#[tokio::test]
async fn undo_of_undo_redoes() {
    let e = env();
    let b = batch_id(&apply(&e, vec![trash(&e, "a.jpg")], false, None).await);
    let u = batch_id(&undo(&e, &b, false).await);
    assert!(e.root.join("a.jpg").exists());
    // A redo of a trash reports like a trash: no place inside `.asteria`.
    let dry = undo(&e, &u, true).await;
    let r = &dry["result"]["results"][0];
    assert_eq!(r["path"], "a.jpg");
    assert!(
        r.get("new_path").is_none() && r.get("new_item_id").is_none(),
        "{r}"
    );
    let v = undo(&e, &u, false).await;
    assert_eq!(status(&v, 0), "done");
    let r = &v["result"]["results"][0];
    assert_eq!(r["path"], "a.jpg");
    assert!(
        r.get("new_path").is_none() && r.get("new_item_id").is_none(),
        "{r}"
    );
    assert!(!e.root.join("a.jpg").exists());
    let redo = &journal(&e)[0];
    assert_eq!(
        redo.begin.redo_of.map(|b| b.to_string()),
        Some(u.clone()),
        "{redo:?}"
    );
}

#[tokio::test]
async fn twice_is_already_undone() {
    let e = env();
    let b = batch_id(&apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, None).await);
    assert!(undo(&e, &b, false).await.get("error").is_none());
    let again = undo(&e, &b, false).await;
    assert_eq!(again["error"]["code"], codes::ALREADY_UNDONE);
    let dry = undo(&e, &b, true).await;
    assert_eq!(dry["error"]["code"], codes::ALREADY_UNDONE);
}

#[tokio::test]
async fn concurrent_undos_of_one_batch_run_once() {
    let e = env();
    let b = batch_id(&apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, None).await);
    let (x, y) = tokio::join!(undo(&e, &b, false), undo(&e, &b, false));
    let ok = [&x, &y].iter().filter(|v| v.get("error").is_none()).count();
    assert_eq!(ok, 1, "{x} {y}");
    for v in [&x, &y] {
        if let Some(err) = v.get("error") {
            let code = err["code"].as_i64().unwrap();
            assert!(code == codes::BUSY as i64 || code == codes::ALREADY_UNDONE as i64);
        }
    }
    let undos = journal(&e)
        .iter()
        .filter(|s| s.begin.undo_of.is_some())
        .count();
    assert_eq!(undos, 1);
}

#[tokio::test]
async fn undo_skips_files_changed_since() {
    let e = env();
    let b = batch_id(
        &apply(
            &e,
            vec![
                mv(&e, "a.jpg", "2025/x.jpg"),
                mv(&e, "sub/b.jpg", "2025/y.jpg"),
            ],
            false,
            None,
        )
        .await,
    );
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(e.root.join("2025/x.jpg"))
        .unwrap();
    std::io::Write::write_all(&mut f, b"more").unwrap();
    drop(f);
    let v = undo(&e, &b, false).await;
    // Newest first: y.jpg goes back, x.jpg is left alone.
    let results = v["result"]["results"].as_array().unwrap();
    let of = |p: &str| results.iter().find(|r| r["path"] == p).unwrap().clone();
    assert_eq!(of("2025/x.jpg")["status"], "skipped");
    assert_eq!(of("2025/x.jpg")["reason"], "changed");
    assert_eq!(of("2025/y.jpg")["status"], "done");
    assert!(e.root.join("sub/b.jpg").exists() && e.root.join("2025/x.jpg").exists());
}

#[tokio::test]
async fn undo_skips_when_the_old_place_is_taken() {
    let e = env();
    let b = batch_id(&apply(&e, vec![mv(&e, "a.jpg", "2025/x.jpg")], false, None).await);
    fs::write(e.root.join("a.jpg"), "someone else").unwrap();
    let v = undo(&e, &b, false).await;
    assert_eq!(status(&v, 0), "skipped");
    assert_eq!(v["result"]["results"][0]["reason"], "target_exists");
    assert_eq!(fs::read(e.root.join("a.jpg")).unwrap(), b"someone else");
    assert!(e.root.join("2025/x.jpg").exists());
}

#[tokio::test]
async fn a_skipped_restore_reports_the_old_place_not_the_trash() {
    let e = env();
    let b = batch_id(&apply(&e, vec![trash(&e, "a.jpg")], false, None).await);
    fs::write(e.root.join("a.jpg"), "someone else").unwrap();
    let v = undo(&e, &b, false).await;
    assert_eq!(status(&v, 0), "skipped");
    assert_eq!(v["result"]["results"][0]["path"], "a.jpg");
}

#[tokio::test]
async fn undo_dry_run_changes_nothing() {
    let e = env();
    let b = batch_id(&apply(&e, vec![mv(&e, "a.jpg", "2025/x.jpg")], false, None).await);
    let n = journal(&e).len();
    let v = undo(&e, &b, true).await;
    assert_eq!(status(&v, 0), "done");
    assert!(v["result"]["batch_id"].is_null());
    assert!(e.root.join("2025/x.jpg").exists() && !e.root.join("a.jpg").exists());
    assert_eq!(journal(&e).len(), n);
}

#[tokio::test]
async fn unknown_batch_is_not_found() {
    let e = env();
    let v = undo(&e, &grain_id::GrainId::random().to_string(), false).await;
    assert_eq!(v["error"]["code"], codes::NOT_FOUND);
    let v = undo(&e, "nonsense", false).await;
    assert_eq!(v["error"]["code"], codes::NOT_FOUND);
}

async fn batch_status(e: &Env, id: &str) -> Value {
    call(
        &e.app,
        &e.owner_token,
        "organize.status",
        json!({"batch_id": id}),
    )
    .await
}

#[tokio::test]
async fn apply_within_the_wait_is_not_running() {
    let e = env();
    let v = apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, None).await;
    assert_eq!(v["result"]["running"], false);
    assert_eq!(v["result"]["results"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn status_of_a_finished_batch_has_its_results() {
    let e = env();
    let v = apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, None).await;
    let s = batch_status(&e, &batch_id(&v)).await;
    let r = &s["result"];
    assert_eq!(r["running"], false, "{s}");
    assert_eq!(r["complete"], true);
    assert_eq!(r["done"], 1);
    assert_eq!(r["skipped"], 0);
    assert_eq!(r["batch_id"], v["result"]["batch_id"]);
    assert_eq!(r["results"], v["result"]["results"]);
}

#[tokio::test]
async fn status_of_an_unknown_batch_is_not_found() {
    let e = env();
    for id in [
        "not an id".to_string(),
        grain_id::GrainId::random().to_string(),
    ] {
        let s = batch_status(&e, &id).await;
        assert_eq!(s["error"]["code"], codes::NOT_FOUND, "{s}");
        assert_eq!(s["error"]["message"], "no such batch");
    }
}

#[tokio::test]
async fn history_pages_newest_first() {
    let e = env();
    let b1 = batch_id(&apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, Some("one")).await);
    let b2 = batch_id(&apply(&e, vec![mv(&e, "sub/b.jpg", "d.jpg")], false, Some("two")).await);
    let b3 = batch_id(&undo(&e, &b1, false).await);

    let hist = |params: Value| {
        let e = &e;
        async move { call(&e.app, &e.owner_token, "organize.history", params).await }
    };
    let p1 = hist(json!({"limit": 2})).await;
    let batches = p1["result"]["batches"].as_array().unwrap();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0]["batch_id"], b3);
    assert_eq!(batches[1]["batch_id"], b2);
    assert_eq!(p1["result"]["next_cursor"], b2);
    assert_eq!(batches[0]["undo_of"], b1);
    assert_eq!(batches[0]["agent"], false);
    assert_eq!(batches[0]["user"], e.lib.owner().to_string());
    assert_eq!(batches[1]["label"], "two");
    assert_eq!(batches[1]["complete"], true);
    // The journal's own count: the move and the batch's closing tidy.
    assert_eq!(batches[1]["done"], 2);
    assert_eq!(batches[1]["op_count"], 2);
    assert!(batches[1]["undone_by"].is_null());

    let p2 = hist(json!({"limit": 2, "cursor": b2})).await;
    let batches = p2["result"]["batches"].as_array().unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["batch_id"], b1);
    assert_eq!(batches[0]["undone_by"], b3);
    assert!(p2["result"]["next_cursor"].is_null());

    let bad = hist(json!({"cursor": "nonsense"})).await;
    assert_eq!(bad["error"]["code"], codes::INVALID_PARAMS);
    let gone = hist(json!({"cursor": grain_id::GrainId::random().to_string()})).await;
    assert_eq!(gone["error"]["code"], codes::INVALID_PARAMS);
}

#[tokio::test]
async fn history_filters_by_kinds_and_labels_unnamed_batches() {
    let e = env();
    let moved = batch_id(&apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, None).await);
    let trashed = batch_id(&apply(&e, vec![trash(&e, "sub/b.jpg")], false, Some("bin")).await);
    let hist = |kinds: Value| {
        let e = &e;
        async move {
            let v = call(
                &e.app,
                &e.owner_token,
                "organize.history",
                json!({"kinds": kinds}),
            )
            .await;
            let ids: Vec<String> = v["result"]["batches"]
                .as_array()
                .unwrap_or_else(|| panic!("{v}"))
                .iter()
                .map(|b| b["batch_id"].as_str().unwrap().to_string())
                .collect();
            (ids, v)
        }
    };
    assert_eq!(
        hist(json!(["trash"])).await.0,
        std::slice::from_ref(&trashed)
    );
    assert_eq!(hist(json!(["move"])).await.0, std::slice::from_ref(&moved));
    assert_eq!(
        hist(json!(["move", "trash"])).await.0,
        [trashed.clone(), moved.clone()]
    );
    assert!(hist(json!(["rate"])).await.0.is_empty());
    let (all, v) = hist(Value::Null).await;
    assert_eq!(all, [trashed, moved]);
    assert_eq!(v["result"]["batches"][0]["label"], "bin");
    assert_eq!(v["result"]["batches"][1]["label"], "Move 1 files");
}

#[tokio::test]
async fn history_with_kinds_pages_over_the_matching_batches() {
    let e = env();
    let t1 = batch_id(&apply(&e, vec![trash(&e, "a.jpg")], false, None).await);
    apply(&e, vec![mv(&e, "sub/b.jpg", "b.jpg")], false, None).await;
    let t2 = batch_id(&apply(&e, vec![trash(&e, "b.jpg")], false, None).await);
    let page = |cursor: Value| {
        let e = &e;
        async move {
            call(
                &e.app,
                &e.owner_token,
                "organize.history",
                json!({"kinds": ["trash"], "limit": 1, "cursor": cursor}),
            )
            .await
        }
    };
    let p1 = page(Value::Null).await;
    assert_eq!(p1["result"]["batches"][0]["batch_id"], t2, "{p1}");
    assert_eq!(p1["result"]["next_cursor"], t2);
    let p2 = page(json!(t2)).await;
    assert_eq!(p2["result"]["batches"][0]["batch_id"], t1, "{p2}");
    assert!(p2["result"]["next_cursor"].is_null(), "{p2}");
}

/// Holds `after_moves`, which runs at the end of every batch, until the gate opens.
#[derive(Default)]
struct Gate {
    closed: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl Gate {
    fn set(&self, closed: bool) {
        *self.closed.lock().unwrap() = closed;
        self.cv.notify_all();
    }
}

impl AppExtension for Gate {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
    fn after_moves(&self, _ctx: &ExtContext, _moves: &[(String, String)]) -> Vec<Rewrite> {
        let guard = self.closed.lock().unwrap();
        let _ = self
            .cv
            .wait_timeout_while(guard, Duration::from_secs(30), |closed| *closed)
            .unwrap();
        Vec::new()
    }
}

#[tokio::test]
async fn a_long_batch_answers_running_then_status_finishes() {
    let gate = Arc::new(Gate::default());
    let e = common::env_ext(gate.clone());
    e.state.set_organize_wait(Duration::from_millis(50));
    gate.set(true);
    let v = apply(&e, vec![mv(&e, "a.jpg", "c.jpg")], false, None).await;
    assert_eq!(v["result"]["running"], true, "{v}");
    assert!(v["result"]["results"].as_array().unwrap().is_empty());
    let id = batch_id(&v);
    let s = batch_status(&e, &id).await;
    assert_eq!(s["result"]["running"], true, "{s}");
    assert_eq!(s["result"]["complete"], false);
    assert!(s["result"].get("results").is_none(), "{s}");

    gate.set(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let s = loop {
        let s = batch_status(&e, &id).await;
        if s["result"]["running"] == false {
            break s;
        }
        assert!(std::time::Instant::now() < deadline, "still running: {s}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let r = &s["result"];
    assert_eq!(r["complete"], true, "{s}");
    assert_eq!(r["done"], 1);
    assert_eq!(r["results"][0]["status"], "done");
    assert_eq!(r["results"][0]["new_path"], "c.jpg");
    assert!(e.root.join("c.jpg").exists() && !Path::new(&e.root.join("a.jpg")).exists());
}
