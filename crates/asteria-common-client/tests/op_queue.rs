//! The rating queue's sender against stub servers: what it sends, when, and what it keeps.

mod common;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use asteria_common_api::{ItemId, RejectReason, codes};
use asteria_common_client::profiles::Token;
use asteria_common_client::queue::{Notice, OpQueue, TestGate, set_retry_backoff, start_for_test};
use corundum_framework::egui;
use serde_json::{Value, json};
use url::Url;

use common::offline::wait_for;
use common::ops::{Calls, all_applied, media_item};
use common::{StubRequest, StubResponse, library_info, serve};

/// A stub answering with `handler`, except `library.info`: the queues' library, `test`, which
/// the sender checks before it sends. Only what reaches `handler` is for it to record.
fn serve_queue(
    handler: impl Fn(&StubRequest) -> StubResponse + Send + Sync + 'static,
) -> SocketAddr {
    serve(move |req| match req.rpc() {
        Some((id, method)) if method == "library.info" => {
            StubResponse::rpc_result(id, library_info("test"))
        }
        _ => handler(req),
    })
}

/// A stub that applies every rating it gets.
fn applying_server() -> (SocketAddr, Calls) {
    let calls = Calls::default();
    let seen = calls.clone();
    let addr = serve_queue(move |req| {
        let (id, _, params) = seen.record(req);
        StubResponse::rpc_result(id, all_applied(&params))
    });
    (addr, calls)
}

fn url(addr: SocketAddr) -> Url {
    Url::parse(&format!("http://{addr}")).unwrap()
}

fn start(addr: SocketAddr, path: &Path, gate_open: bool) -> (OpQueue, TestGate) {
    start_for_test(
        url(addr),
        Token::new("t"),
        path.to_owned(),
        egui::Context::default(),
        gate_open,
    )
}

fn queue_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("queue").join("lib").join("profile.jsonl")
}

fn file_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn item(s: &str) -> ItemId {
    ItemId(s.into())
}

#[test]
fn rated_ops_are_sent_and_the_file_empties() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let (addr, calls) = applying_server();
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 4);
    wait_for("the rating to be confirmed", || queue.pending() == 0);
    assert_eq!(calls.batches().len(), 1);
    assert_eq!(calls.batches()[0][0]["item"], "A");
    assert_eq!(calls.batches()[0][0]["value"], 4);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(queue.rating(&item("A")), Some(4));
    assert!(queue.take_notices().is_empty());
}

#[test]
fn nothing_is_sent_while_offline_and_all_goes_on_wake() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let (addr, calls) = applying_server();
    let (queue, gate) = start(addr, &path, false);
    gate.offline();
    queue.rate(item("A"), 1);
    queue.rate(item("B"), 2);
    // Shown at once, and kept on disk.
    assert_eq!(queue.rating(&item("A")), Some(1));
    assert_eq!(queue.pending(), 2);
    wait_for("both ratings to be saved", || file_lines(&path).len() == 2);
    std::thread::sleep(Duration::from_millis(300));
    assert!(calls.all().is_empty());
    gate.open();
    gate.wake();
    wait_for("both ratings to be confirmed", || queue.pending() == 0);
    let batches = calls.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), 2);
}

#[test]
fn an_unreachable_server_keeps_ops_and_retries() {
    set_retry_backoff(Duration::from_millis(50), Duration::from_millis(200));
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let calls = Calls::default();
    let seen = calls.clone();
    let answer = Arc::new(AtomicBool::new(false));
    let answering = answer.clone();
    let addr = serve_queue(move |req| {
        let (id, _, params) = seen.record(req);
        if answering.load(Ordering::SeqCst) {
            StubResponse::rpc_result(id, all_applied(&params))
        } else {
            StubResponse::hang_up()
        }
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 3);
    wait_for("two failed attempts", || calls.count("ops.apply") >= 2);
    let lines = file_lines(&path);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("\"add\""), "{lines:?}");
    assert_eq!(queue.pending(), 1);
    assert_eq!(queue.rating(&item("A")), Some(3));
    answer.store(true, Ordering::SeqCst);
    wait_for("the retry to get through", || queue.pending() == 0);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(queue.rating(&item("A")), Some(3));
}

#[test]
fn a_large_backlog_goes_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let (addr, calls) = applying_server();
    let (queue, gate) = start(addr, &path, false);
    gate.offline();
    for n in 0..450 {
        queue.rate(item(&format!("item-{n}")), (n % 5) as i8 + 1);
    }
    assert_eq!(queue.pending(), 450);
    // The appender writes on its own thread; the backlog is the file, so wait for all of it.
    wait_for("the backlog to reach the file", || {
        file_lines(&path).len() == 450
    });
    gate.open();
    gate.wake();
    wait_for("the backlog to be confirmed", || queue.pending() == 0);
    let sizes: Vec<usize> = calls.batches().iter().map(Vec::len).collect();
    assert_eq!(sizes, [200, 200, 50]);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(queue.rating(&item("item-7")), Some(3));
}

#[test]
fn rejected_ops_are_dropped_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let addr = serve_queue(move |req| {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let (id, _) = req.rpc().unwrap();
        let op = &body["params"]["ops"][0];
        StubResponse::rpc_result(
            id,
            json!({"results": [{"id": op["id"], "status": "rejected", "reason": "read_only"}]}),
        )
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 2);
    wait_for("the rating to be settled", || queue.pending() == 0);
    assert_eq!(
        queue.take_notices(),
        [Notice::RatingRejected {
            item: item("A"),
            reason: RejectReason::ReadOnly,
        }]
    );
    assert!(queue.take_notices().is_empty());
    assert_eq!(queue.rating(&item("A")), None);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
}

#[test]
fn superseded_shows_the_servers_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let addr = serve_queue(move |req| {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let (id, _) = req.rpc().unwrap();
        let op = &body["params"]["ops"][0];
        StubResponse::rpc_result(
            id,
            json!({"results": [{
                "id": op["id"],
                "status": "superseded",
                "item": media_item("A", 5),
            }]}),
        )
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 2);
    wait_for("the rating to be settled", || queue.pending() == 0);
    assert_eq!(queue.rating(&item("A")), Some(5));
}

#[test]
fn an_old_server_gets_media_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let calls = Calls::default();
    let seen = calls.clone();
    let addr = serve_queue(move |req| {
        let (id, method, params) = seen.record(req);
        match method.as_str() {
            "media.update" => StubResponse::rpc_result(
                id,
                media_item(
                    params["id"].as_str().unwrap(),
                    params["rating"].as_i64().unwrap(),
                ),
            ),
            _ => StubResponse::rpc_error(id, codes::METHOD_NOT_FOUND, "method not found"),
        }
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 4);
    queue.rate(item("B"), 1);
    wait_for("both ratings to be confirmed", || queue.pending() == 0);
    assert_eq!(calls.count("ops.apply"), 1);
    assert_eq!(calls.count("media.update"), 2);
    assert_eq!(queue.rating(&item("A")), Some(4));
    assert_eq!(queue.rating(&item("B")), Some(1));
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
}

#[test]
fn ops_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let (addr, calls) = applying_server();
    let (queue, gate) = start(addr, &path, false);
    gate.offline();
    queue.rate(item("A"), 5);
    drop(queue);
    let (queue, _gate) = start(addr, &path, true);
    wait_for("the saved rating to be sent", || {
        calls.batches().len() == 1 && queue.pending() == 0
    });
    let batches = calls.batches();
    assert_eq!(batches[0].len(), 1);
    assert_eq!(batches[0][0]["item"], "A");
    assert_eq!(batches[0][0]["value"], 5);
}

#[test]
fn a_second_queue_waits_for_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let (addr, calls) = applying_server();
    let (first, gate) = start(addr, &path, false);
    gate.offline();
    first.rate(item("A"), 1);
    wait_for("the first rating to be saved", || {
        file_lines(&path).len() == 1
    });
    let (second, _gate2) = start(addr, &path, true);
    second.rate(item("B"), 2);
    assert_eq!(second.rating(&item("B")), Some(2));
    std::thread::sleep(Duration::from_millis(300));
    assert!(calls.all().is_empty());
    drop(first);
    wait_for("the second queue to send both", || second.pending() == 0);
    let sent: usize = calls.batches().iter().map(Vec::len).sum();
    assert_eq!(sent, 2);
}

#[test]
fn an_unwritable_file_notices_not_saved() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    fs::write(&blocker, b"a file, not a directory").unwrap();
    let path = blocker.join("profile.jsonl");
    let (addr, calls) = applying_server();
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 3);
    let mut notices = Vec::new();
    wait_for("a notice", || {
        notices.extend(queue.take_notices());
        !notices.is_empty()
    });
    assert_eq!(notices, [Notice::RatingNotSaved { item: item("A") }]);
    assert_eq!(queue.rating(&item("A")), None);
    assert_eq!(queue.pending(), 0);
    assert!(calls.all().is_empty());
}

#[test]
fn a_newer_click_during_a_send_keeps_showing() {
    // The first ops.apply is held until the second click is queued; its `applied` must not
    // hide the newer pending value, which then goes out too.
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let calls = Calls::default();
    let seen = calls.clone();
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let arrived = Arc::new(AtomicUsize::new(0));
    let arrivals = arrived.clone();
    let addr = serve_queue(move |req| {
        let (id, _, params) = seen.record(req);
        if arrivals.fetch_add(1, Ordering::SeqCst) == 0 {
            while !released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        StubResponse::rpc_result(id, all_applied(&params))
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 2);
    wait_for("the first send", || arrived.load(Ordering::SeqCst) == 1);
    queue.rate(item("A"), 5);
    release.store(true, Ordering::SeqCst);
    wait_for("the first answer", || calls.count("ops.apply") == 2);
    wait_for("the second answer", || queue.pending() == 0);
    assert_eq!(queue.rating(&item("A")), Some(5));
    let batches = calls.batches();
    assert_eq!(batches[1][0]["value"], 5);
}

#[test]
fn a_new_rating_ends_a_running_backoff() {
    // The first ops.apply is told to come back in 30 s; a rating made meanwhile goes at once.
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let calls = Calls::default();
    let seen = calls.clone();
    let addr = serve_queue(move |req| {
        let (id, _, params) = seen.record(req);
        if seen.count("ops.apply") == 1 {
            StubResponse::new(503, Vec::new()).header("retry-after", "30")
        } else {
            StubResponse::rpc_result(id, all_applied(&params))
        }
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 1);
    wait_for("the first attempt", || calls.count("ops.apply") == 1);
    std::thread::sleep(Duration::from_millis(100));
    queue.rate(item("B"), 2);
    let start = std::time::Instant::now();
    wait_for("both ratings to be confirmed", || queue.pending() == 0);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    let batches = calls.batches();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[1].len(), 2);
}

#[test]
fn an_old_server_refusing_one_item_still_rates_the_next() {
    // `media.update` fails for A with an error of its own; that refuses A alone.
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let calls = Calls::default();
    let seen = calls.clone();
    let addr = serve_queue(move |req| {
        let (id, method, params) = seen.record(req);
        match method.as_str() {
            "media.update" if params["id"] == "A" => {
                StubResponse::rpc_error(id, codes::INTERNAL_ERROR, "cannot write the sidecar")
            }
            "media.update" => StubResponse::rpc_result(
                id,
                media_item(
                    params["id"].as_str().unwrap(),
                    params["rating"].as_i64().unwrap(),
                ),
            ),
            _ => StubResponse::rpc_error(id, codes::METHOD_NOT_FOUND, "method not found"),
        }
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 4);
    queue.rate(item("B"), 1);
    wait_for("both ratings to be settled", || queue.pending() == 0);
    assert_eq!(
        queue.take_notices(),
        [Notice::RatingRejected {
            item: item("A"),
            reason: RejectReason::ReadOnly,
        }]
    );
    assert_eq!(queue.rating(&item("A")), None);
    assert_eq!(queue.rating(&item("B")), Some(1));
    assert_eq!(calls.count("media.update"), 2);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
}

#[test]
fn a_rating_during_a_held_send_reaches_the_file_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = queue_file(&dir);
    let calls = Calls::default();
    let seen = calls.clone();
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let addr = serve_queue(move |req| {
        let (id, _, params) = seen.record(req);
        while !released.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        StubResponse::rpc_result(id, all_applied(&params))
    });
    let (queue, _gate) = start(addr, &path, true);
    queue.rate(item("A"), 2);
    wait_for("the first send", || calls.count("ops.apply") == 1);
    queue.rate(item("B"), 4);
    // Saved while the send is still held, not once it returns.
    let start = std::time::Instant::now();
    while !file_lines(&path).iter().any(|l| l.contains("\"B\"")) {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "B never reached the file: {:?}",
            file_lines(&path)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!release.load(Ordering::SeqCst));
    release.store(true, Ordering::SeqCst);
    wait_for("both ratings to be confirmed", || queue.pending() == 0);
}
