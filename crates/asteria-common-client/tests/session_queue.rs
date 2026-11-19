//! `Session::rate`: ratings go through the session's queue, online or offline, and stay with
//! their profile and library.

mod common;
mod server;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use asteria_common_api::{
    ItemId, LibraryInfo, MediaGetParams, MediaItem, MediaKind, RejectReason, codes,
};
use asteria_common_client::offline::{KnownLibraries, OfflineStore, Pin, PinStatus};
use asteria_common_client::profiles::{ServerProfile, Token};
use asteria_common_client::queue::{Notice, queue_path, start_for_test};
use asteria_common_client::rpc::ClientError;
use asteria_common_client::session::{Request, Response, Session, SessionDirs, SessionState};
use corundum_framework::egui;

use common::offline::{FakeCatalog, set_pin, wait_for};
use common::ops::{Calls, all_applied};
use common::{
    DEADLINE, StubResponse, free_addr, library_info, photos_client, photos_server_info, profile,
    serve_at, wait_settled,
};
use server::TestServer;

/// The library the stubs serve.
const LIB: &str = "lib-l";

/// How long a test watches for something that must not happen.
const QUIET: std::time::Duration = std::time::Duration::from_millis(500);

fn dirs(data: &Path) -> SessionDirs {
    SessionDirs {
        cache: None,
        data: Some(data.to_path_buf()),
    }
}

/// Opens `profile` with a data root and a catalog, so it can work offline, and waits for the
/// handshake to settle.
fn open(profile: &ServerProfile, data: &Path) -> Session {
    let mut session = Session::open_with(
        profile,
        photos_client(),
        egui::Context::default(),
        dirs(data),
        Some(Arc::new(FakeCatalog)),
    );
    wait_settled(&mut session);
    session
}

/// Records `library_id` as what `profile` last saw, with a pin, so a session that cannot reach
/// the server opens offline.
fn known(data: &Path, profile: &ServerProfile, library_id: &str) {
    let library = LibraryInfo {
        library_id: library_id.into(),
        owner: "owner".into(),
        epoch: "e1".into(),
        item_count: 0,
        scanning: false,
        revision: 0,
    };
    KnownLibraries::new(data)
        .record(profile.id, &library)
        .unwrap();
    OfflineStore::open(data, library_id)
        .unwrap()
        .save_pins(&[Pin {
            target: set_pin("s1"),
            added_unix: 0,
            status: PinStatus::Pending,
        }])
        .unwrap();
}

fn open_offline(profile: &ServerProfile, data: &Path) -> Session {
    let session = open(profile, data);
    assert!(
        matches!(session.state(), SessionState::Offline { .. }),
        "{:?}",
        session.state()
    );
    session
}

/// Polls `session` until `done` holds; fails the test after [`DEADLINE`].
fn poll_until(session: &mut Session, what: &str, mut done: impl FnMut(&mut Session) -> bool) {
    let start = Instant::now();
    loop {
        session.poll();
        if done(session) {
            return;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "timed out waiting for {what}: {:?}",
            session.state()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn wait_ready(session: &mut Session) {
    poll_until(session, "Ready", |s| {
        matches!(s.state(), SessionState::Ready { .. })
    });
}

fn media_get(session: &mut Session, id: &ItemId) -> MediaItem {
    let request = session.request(Request::MediaGet(MediaGetParams { id: id.clone() }));
    let start = Instant::now();
    let reply = loop {
        if let Some(reply) = session.poll().into_iter().find(|r| r.id == request) {
            break reply;
        }
        assert!(start.elapsed() < DEADLINE, "media.get never answered");
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    match reply.result {
        Ok(Response::Media(item)) => item,
        other => panic!("expected an item, got {other:?}"),
    }
}

/// An item `id` as a listing would show it, with `rating`.
fn shown(id: &str, rating: Option<i8>) -> MediaItem {
    MediaItem {
        id: ItemId(id.into()),
        path: format!("{id}.jpg"),
        kind: MediaKind::Image,
        size: 1,
        modified: "2026-11-16T00:00:00Z".into(),
        rating,
        subjects: Vec::new(),
        hierarchical_subjects: Vec::new(),
        description: None,
        label: None,
        content_hash: None,
        file_hash: None,
        read_only_reason: None,
    }
}

fn item(id: &str) -> ItemId {
    ItemId(id.into())
}

fn queue_file(data: &Path, profile: &ServerProfile) -> PathBuf {
    queue_path(data, LIB, &profile.id.to_string()).unwrap()
}

fn file_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A stub that answers the handshake for `library_id` and applies every rating, recording the
/// calls. With `hold`, the first `ops.apply` waits until the hold is released.
struct RatingStub {
    calls: Calls,
    /// `ops.apply` calls that have arrived (answered or held).
    arrived: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
}

impl RatingStub {
    fn start_at(addr: SocketAddr, library_id: &'static str, hold: bool) -> RatingStub {
        let calls = Calls::default();
        let arrived = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(!hold));
        let (seen, arrivals, released) = (calls.clone(), arrived.clone(), release.clone());
        serve_at(addr, move |req| {
            let (id, method, params) = seen.record(req);
            match method.as_str() {
                "server.info" => StubResponse::rpc_result(id, photos_server_info()),
                "library.info" => StubResponse::rpc_result(id, library_info(library_id)),
                "ops.apply" => {
                    if arrivals.fetch_add(1, Ordering::SeqCst) == 0 {
                        let start = Instant::now();
                        while !released.load(Ordering::SeqCst) {
                            assert!(start.elapsed() < DEADLINE, "never released");
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                    StubResponse::rpc_result(id, all_applied(&params))
                }
                // The offline sync asks for its pin; this stub has none.
                _ => StubResponse::rpc_error(id, codes::METHOD_NOT_FOUND, "method not found"),
            }
        });
        RatingStub {
            calls,
            arrived,
            release,
        }
    }

    /// Every rating sent with `ops.apply`, as `(item, value)`, in order.
    fn sent(&self) -> Vec<(String, i64)> {
        self.calls
            .batches()
            .into_iter()
            .flatten()
            .map(|op| {
                (
                    op["item"].as_str().unwrap().to_string(),
                    op["value"].as_i64().unwrap(),
                )
            })
            .collect()
    }
}

#[test]
fn a_rating_reaches_the_server() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let mut session = Session::open_with_dirs(
        &server.profile(&server.token),
        photos_client(),
        egui::Context::default(),
        dirs(data.path()),
    );
    wait_ready(&mut session);
    let a = server.item_id("a.jpg");
    let old = media_get(&mut session, &a);
    assert_ne!(old.rating, Some(4));

    session.rate(&a, 4).unwrap();
    assert_eq!(session.rating_of(&old), Some(4));
    poll_until(&mut session, "the rating to be confirmed", |s| {
        s.pending_ratings() == 0
    });
    assert_eq!(media_get(&mut session, &a).rating, Some(4));
    assert_eq!(session.rating_of(&old), Some(4));
    assert!(session.take_notices().is_empty());
}

#[test]
fn rating_offline_shows_at_once_and_arrives_after_reconnect() {
    let server = TestServer::start();
    let a = server.item_id("a.jpg");
    let data = tempfile::tempdir().unwrap();
    let addr = free_addr();
    let profile = profile(addr, &server.token);
    known(data.path(), &profile, &server.library_id);
    let mut session = open_offline(&profile, data.path());

    session.rate(&a, 3).unwrap();
    assert_eq!(session.rating_of(&shown(&a.0, None)), Some(3));
    assert_eq!(session.pending_ratings(), 1);

    server.listen_at(addr);
    session.reconnect();
    wait_ready(&mut session);
    poll_until(&mut session, "the rating to be sent", |s| {
        s.pending_ratings() == 0
    });
    assert_eq!(media_get(&mut session, &a).rating, Some(3));
    assert!(session.take_notices().is_empty());
}

#[test]
fn last_click_wins_across_reconnect() {
    let data = tempfile::tempdir().unwrap();
    let addr = free_addr();
    let profile = profile(addr, "t");
    known(data.path(), &profile, LIB);
    let mut session = open_offline(&profile, data.path());
    let a = shown("A", None);

    for value in [1, 2, 3] {
        session.rate(&a.id, value).unwrap();
    }
    assert_eq!(session.rating_of(&a), Some(3));

    let stub = RatingStub::start_at(addr, LIB, true);
    session.reconnect();
    poll_until(&mut session, "the first ops.apply", |s| {
        assert_eq!(s.rating_of(&a), Some(3));
        stub.arrived.load(Ordering::SeqCst) == 1
    });
    session.rate(&a.id, 5).unwrap();
    assert_eq!(session.rating_of(&a), Some(5));
    stub.release.store(true, Ordering::SeqCst);
    poll_until(&mut session, "the last click to be confirmed", |s| {
        // Never the held batch's 3 once 5 was clicked.
        assert_eq!(s.rating_of(&a), Some(5));
        s.pending_ratings() == 0
    });
    assert_eq!(session.rating_of(&a), Some(5));
    let sent = stub.sent();
    // The offline clicks went as one rating, the newest; the last click went last.
    assert_eq!(sent[0], ("A".to_string(), 3));
    assert_eq!(sent.last().unwrap(), &("A".to_string(), 5));
    assert!(session.take_notices().is_empty());
}

#[test]
fn pending_ratings_stay_with_their_profile() {
    let data = tempfile::tempdir().unwrap();
    let addr1 = free_addr();
    let p1 = profile(addr1, "t1");
    known(data.path(), &p1, LIB);
    let mut s1 = open_offline(&p1, data.path());
    s1.rate(&item("A"), 4).unwrap();
    wait_for("P1's rating to be saved", || {
        file_lines(&queue_file(data.path(), &p1)).len() == 1
    });
    drop(s1);

    // Another profile on the same library: its queue sends its own rating, not P1's.
    let addr2 = free_addr();
    let stub2 = RatingStub::start_at(addr2, LIB, false);
    let p2 = profile(addr2, "t2");
    let mut s2 = open(&p2, data.path());
    wait_ready(&mut s2);
    s2.rate(&item("B"), 2).unwrap();
    poll_until(&mut s2, "P2's rating to be confirmed", |s| {
        s.pending_ratings() == 0
    });
    assert_eq!(stub2.sent(), [("B".to_string(), 2)]);
    drop(s2);
    assert_eq!(file_lines(&queue_file(data.path(), &p1)).len(), 1);

    // P1 again, with its server up: its rating goes there.
    let stub1 = RatingStub::start_at(addr1, LIB, false);
    let mut s1 = open(&p1, data.path());
    wait_ready(&mut s1);
    poll_until(&mut s1, "P1's rating to be sent", |s| {
        s.pending_ratings() == 0 && !stub1.sent().is_empty()
    });
    assert_eq!(stub1.sent(), [("A".to_string(), 4)]);
    assert_eq!(stub2.sent(), [("B".to_string(), 2)]);
}

#[test]
fn library_changed_stops_sending_and_keeps_the_file() {
    let data = tempfile::tempdir().unwrap();
    let addr = free_addr();
    let profile = profile(addr, "t");
    known(data.path(), &profile, LIB);
    let mut session = open_offline(&profile, data.path());
    session.rate(&item("A"), 2).unwrap();
    let path = queue_file(data.path(), &profile);
    wait_for("the rating to be saved", || file_lines(&path).len() == 1);

    let stub = RatingStub::start_at(addr, "another-library", false);
    session.reconnect();
    poll_until(&mut session, "Failed", |s| {
        matches!(s.state(), SessionState::Failed(_))
    });
    assert_eq!(
        session.state(),
        &SessionState::Failed(ClientError::LibraryChanged)
    );
    // A new rating goes at once, if anything does: it is kept, and not sent either.
    session.rate(&item("B"), 5).unwrap();
    wait_for("the second rating to be saved", || {
        file_lines(&path).len() == 2
    });
    // Nothing can show that something was not sent but time: long enough for a send to start.
    let quiet = Instant::now();
    while quiet.elapsed() < QUIET {
        session.poll();
        assert_eq!(stub.calls.count("ops.apply"), 0);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    drop(session);

    // A later queue on the file gets it only once the session's queue has let go of it, so
    // from here on nothing more can come from that queue.
    let (later, _gate) = start_for_test(
        url::Url::parse(&format!("http://{addr}")).unwrap(),
        Token::new("t"),
        path.clone(),
        egui::Context::default(),
        false,
    );
    wait_for("the later queue to load the file", || later.pending() == 2);
    assert_eq!(stub.calls.count("ops.apply"), 0);
    let lines = file_lines(&path);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().all(|l| l.contains("\"add\"")), "{lines:?}");
}

#[test]
fn rate_without_a_data_root_is_offline_error() {
    let server = TestServer::start();
    let mut session = Session::open(
        &server.profile(&server.token),
        photos_client(),
        egui::Context::default(),
    );
    wait_ready(&mut session);
    assert!(session.ops().is_none());
    assert_eq!(
        session.rate(&server.item_id("a.jpg"), 3),
        Err(ClientError::Offline)
    );
    assert_eq!(session.pending_ratings(), 0);
}

#[test]
fn refused_rating_becomes_a_notice() {
    let server = TestServer::start_with(|root| {
        // A broken sidecar makes the item read-only.
        fs::write(root.join("a.jpg.xmp"), "<not xml").unwrap();
    });
    let data = tempfile::tempdir().unwrap();
    let mut session = Session::open_with_dirs(
        &server.profile(&server.token),
        photos_client(),
        egui::Context::default(),
        dirs(data.path()),
    );
    wait_ready(&mut session);
    let a = server.item_id("a.jpg");
    session.rate(&a, 4).unwrap();
    let mut notices = Vec::new();
    poll_until(&mut session, "a notice", |s| {
        notices.extend(s.take_notices());
        !notices.is_empty()
    });
    assert_eq!(
        notices,
        [Notice::RatingRejected {
            item: a,
            reason: RejectReason::ReadOnly,
        }]
    );
}

#[test]
fn dropping_the_session_during_a_held_send_saves_the_newer_rating() {
    let data = tempfile::tempdir().unwrap();
    let addr = free_addr();
    let stub = RatingStub::start_at(addr, LIB, true);
    let profile = profile(addr, "t");
    let mut session = open(&profile, data.path());
    wait_ready(&mut session);
    session.rate(&item("A"), 2).unwrap();
    poll_until(&mut session, "the held send", |_| {
        stub.arrived.load(Ordering::SeqCst) == 1
    });
    session.rate(&item("B"), 4).unwrap();
    // The app quits while the send is held: the drop waits for the file, not the server.
    let start = Instant::now();
    drop(session);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    let lines = file_lines(&queue_file(data.path(), &profile));
    assert!(lines.iter().any(|l| l.contains("\"B\"")), "{lines:?}");
    stub.release.store(true, Ordering::SeqCst);
}

#[test]
fn a_library_switch_while_ready_keeps_the_queue() {
    let data = tempfile::tempdir().unwrap();
    let addr = free_addr();
    let profile = profile(addr, "t");
    known(data.path(), &profile, LIB);
    let mut session = open_offline(&profile, data.path());
    session.rate(&item("A"), 2).unwrap();
    let path = queue_file(data.path(), &profile);
    wait_for("the rating to be saved", || file_lines(&path).len() == 1);

    // The handshake finds the recorded library; the server has switched by the time the
    // queue sends.
    let calls = Calls::default();
    let seen = calls.clone();
    let infos = Arc::new(AtomicUsize::new(0));
    serve_at(addr, move |req| {
        let (id, method, params) = seen.record(req);
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" if infos.fetch_add(1, Ordering::SeqCst) == 0 => {
                StubResponse::rpc_result(id, library_info(LIB))
            }
            "library.info" => StubResponse::rpc_result(id, library_info("another-library")),
            "ops.apply" => StubResponse::rpc_result(id, all_applied(&params)),
            _ => StubResponse::rpc_error(id, codes::METHOD_NOT_FOUND, "method not found"),
        }
    });
    session.reconnect();
    poll_until(&mut session, "Failed", |s| {
        matches!(s.state(), SessionState::Failed(_))
    });
    assert_eq!(
        session.state(),
        &SessionState::Failed(ClientError::LibraryChanged)
    );
    assert_eq!(calls.count("ops.apply"), 0);
    let lines = file_lines(&path);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("\"add\""), "{lines:?}");
}
