//! A session whose server is gone: it opens offline on the library the profile last saw,
//! answers from the saved copies, and comes back when the server does.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use asteria_common_api::MediaUpdateParams;
use asteria_common_client::offline::{OfflineHandle, PinStatus, SyncPhase};
use asteria_common_client::profiles::ServerProfile;
use asteria_common_client::rpc::ClientError;
use asteria_common_client::session::{
    Reply, Request, RequestId, Response, Session, SessionDirs, SessionState,
};
use corundum_framework::egui;
use serde_json::json;
use url::Url;

use common::offline::{FakeCatalog, FakeServer, LIBRARY_ID, file_hash, set_pin, wait_for};
use common::{
    DEADLINE, StubRequest, StubResponse, free_addr, library_info, photos_client,
    photos_server_info, profile, serve, serve_at, wait_settled,
};

fn open(profile: &ServerProfile, data: &Path) -> Session {
    let mut session = Session::open_with(
        profile,
        photos_client(),
        egui::Context::default(),
        SessionDirs {
            cache: None,
            data: Some(data.to_path_buf()),
        },
        Some(Arc::new(FakeCatalog)),
    );
    wait_settled(&mut session);
    session
}

/// `profile`, moved to `addr`.
fn at(profile: &ServerProfile, addr: SocketAddr) -> ServerProfile {
    ServerProfile {
        url: Url::parse(&format!("http://{addr}")).unwrap(),
        ..profile.clone()
    }
}

fn handle(session: &Session) -> OfflineHandle {
    session.offline().expect("no offline handle").clone()
}

/// Pins `set` and waits until it and the media of `items` are saved.
fn save(session: &mut Session, set: &str, items: &[&str]) {
    let h = handle(session);
    h.pin(set_pin(set));
    wait_for(set, || {
        session.poll();
        h.status("set", set) == Some(PinStatus::Saved)
            && h.progress().phase == SyncPhase::Idle
            && items
                .iter()
                .all(|i| h.media(&file_hash(i), "thumb").is_some())
    });
}

/// Connects once to a fake server (recording its library for the profile), saving the set
/// `s1` of items `a` and `b`. Returns the profile and the server.
fn connect_once(data: &Path) -> (ServerProfile, FakeServer) {
    let server = FakeServer::start();
    server.set("s1", &["a", "b"]);
    let profile = profile(server.addr, "t");
    let mut session = open(&profile, data);
    assert!(
        matches!(session.state(), SessionState::Ready { .. }),
        "{:?}",
        session.state()
    );
    save(&mut session, "s1", &["a", "b"]);
    (profile, server)
}

fn wait_replies(session: &mut Session, ids: &[RequestId]) -> Vec<Reply> {
    let start = Instant::now();
    let mut replies = Vec::new();
    while replies.len() < ids.len() {
        replies.extend(session.poll().into_iter().filter(|r| ids.contains(&r.id)));
        assert!(start.elapsed() < DEADLINE, "replies never came");
        std::thread::sleep(Duration::from_millis(5));
    }
    replies.sort_by_key(|r| ids.iter().position(|id| *id == r.id));
    replies
}

fn set_get(id: &str) -> Request {
    Request::Call {
        method: "set.get".into(),
        params: json!({ "id": id }),
    }
}

fn wait_state(session: &mut Session, what: &str, done: impl Fn(&SessionState) -> bool) {
    let start = Instant::now();
    loop {
        session.poll();
        if done(session.state()) {
            return;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "never {what}: {:?}",
            session.state()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A stub that answers the handshake for `library_id` on `addr`.
fn handshake_stub_at(addr: SocketAddr, library_id: &'static str) {
    serve_at(addr, move |req: &StubRequest| {
        let (id, method) = req.rpc().expect("the stub only answers /rpc");
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" => StubResponse::rpc_result(id, library_info(library_id)),
            other => panic!("stub got unexpected method {other}"),
        }
    });
}

#[test]
fn an_unreachable_server_with_a_known_library_is_offline() {
    let data = tempfile::tempdir().unwrap();
    let (profile, _server) = connect_once(data.path());

    let session = open(&at(&profile, free_addr()), data.path());
    match session.state() {
        SessionState::Offline { library } => assert_eq!(library.library_id, LIBRARY_ID),
        other => panic!("expected Offline, got {other:?}"),
    }
    assert!(session.is_offline());
    assert!(session.offline().is_some());
}

#[test]
fn an_unknown_library_still_fails() {
    let data = tempfile::tempdir().unwrap();
    let session = open(&profile(free_addr(), "t"), data.path());
    assert!(
        matches!(
            session.state(),
            SessionState::Failed(ClientError::Unreachable(_))
        ),
        "{:?}",
        session.state()
    );
    assert!(!session.is_offline());
}

#[test]
fn unauthorized_is_not_offline() {
    let data = tempfile::tempdir().unwrap();
    let (profile, _server) = connect_once(data.path());
    let refusing = serve(|_: &StubRequest| StubResponse::new(401, Vec::new()));

    let session = open(&at(&profile, refusing), data.path());
    assert_eq!(
        session.state(),
        &SessionState::Failed(ClientError::Unauthorized)
    );
}

#[test]
fn offline_answers_come_from_the_copies() {
    let data = tempfile::tempdir().unwrap();
    let (profile, _server) = connect_once(data.path());
    let mut session = open(&at(&profile, free_addr()), data.path());
    assert!(matches!(session.state(), SessionState::Offline { .. }));

    let ids = [
        session.request(set_get("s1")),
        session.request(set_get("s2")),
        session.request(Request::MediaUpdate(MediaUpdateParams {
            id: asteria_common_api::ItemId("a".into()),
            rating: None,
            subjects: None,
            hierarchical_subjects: None,
            description: None,
            label: None,
        })),
        session.request(Request::LibraryInfo),
    ];
    let replies = wait_replies(&mut session, &ids);
    match &replies[0].result {
        Ok(Response::Value(v)) => assert_eq!(v["items"][0]["id"], "a"),
        other => panic!("expected the saved set, got {other:?}"),
    }
    assert_eq!(replies[1].result, Err(ClientError::NotSaved));
    assert_eq!(replies[2].result, Err(ClientError::Offline));
    match &replies[3].result {
        Ok(Response::LibraryInfo(info)) => assert_eq!(info.library_id, LIBRARY_ID),
        other => panic!("expected the recorded library, got {other:?}"),
    }
}

#[test]
fn the_session_comes_back_when_the_server_does() {
    let data = tempfile::tempdir().unwrap();
    let (profile, _server) = connect_once(data.path());
    let addr = free_addr();
    let mut session = open(&at(&profile, addr), data.path());
    assert!(matches!(session.state(), SessionState::Offline { .. }));

    // Still gone: the session stays offline.
    session.reconnect();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        session.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(session.state(), SessionState::Offline { .. }));

    let back = FakeServer::start_at(addr);
    back.set("s1", &["a", "b"]);
    session.reconnect();
    wait_state(&mut session, "Ready", |s| {
        matches!(s, SessionState::Ready { .. })
    });
    assert!(!session.is_offline());
    // The same handle, now online: requests go to the server again.
    let id = session.request(set_get("s1"));
    assert!(wait_replies(&mut session, &[id])[0].result.is_ok());
    assert!(back.requests("set.get s1") >= 1);
}

#[test]
fn reconnecting_to_another_library_fails() {
    let data = tempfile::tempdir().unwrap();
    let (profile, _server) = connect_once(data.path());
    let addr = free_addr();
    let mut session = open(&at(&profile, addr), data.path());
    assert!(matches!(session.state(), SessionState::Offline { .. }));

    handshake_stub_at(addr, "another-library");
    session.reconnect();
    wait_state(&mut session, "Failed", |s| {
        matches!(s, SessionState::Failed(_))
    });
    assert_eq!(
        session.state(),
        &SessionState::Failed(ClientError::LibraryChanged)
    );
    // The old copies are not served as the new library's.
    let id = session.request(set_get("s1"));
    assert_eq!(
        wait_replies(&mut session, &[id])[0].result,
        Err(ClientError::LibraryChanged)
    );
}

#[test]
fn a_server_gone_while_ready_is_answered_from_the_copies() {
    let data = tempfile::tempdir().unwrap();
    let server = FakeServer::start();
    server.set("s1", &["a", "b"]);
    let mut session = open(&profile(server.addr, "t"), data.path());
    assert!(matches!(session.state(), SessionState::Ready { .. }));
    save(&mut session, "s1", &["a", "b"]);

    server.gone(true);
    let ids = [
        session.request(set_get("s1")),
        session.request(set_get("s2")),
    ];
    let replies = wait_replies(&mut session, &ids);
    assert!(replies[0].result.is_ok(), "{:?}", replies[0]);
    assert!(
        matches!(replies[1].result, Err(ClientError::Unreachable(_))),
        "{:?}",
        replies[1]
    );
    assert!(matches!(
        session.degraded(),
        Some(ClientError::Unreachable(_))
    ));
    assert!(session.is_offline());
    assert!(matches!(session.state(), SessionState::Ready { .. }));
}

#[test]
fn dropping_the_session_stops_the_sync() {
    let data = tempfile::tempdir().unwrap();
    let server = FakeServer::start();
    let items: Vec<String> = (0..20).map(|i| format!("i{i}")).collect();
    let items: Vec<&str> = items.iter().map(String::as_str).collect();
    server.set("big", &items);
    server.media_delay(Duration::from_secs(1));
    let session = open(&profile(server.addr, "t"), data.path());
    assert!(matches!(session.state(), SessionState::Ready { .. }));
    handle(&session).pin(set_pin("big"));
    wait_for("the first download", || server.requests("GET /media/") >= 1);

    drop(session);
    // The downloads under way finish (each takes a second); no other starts.
    std::thread::sleep(Duration::from_secs(2));
    let after = server.requests("GET /media/");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(server.requests("GET /media/"), after);
    assert!(after < items.len(), "{after} downloads");
}

#[test]
fn recovering_from_a_degraded_server_refetches_the_listing() {
    let data = tempfile::tempdir().unwrap();
    let server = FakeServer::start();
    server.set("s1", &["a", "b"]);
    let mut session = open(&profile(server.addr, "t"), data.path());
    assert!(matches!(session.state(), SessionState::Ready { .. }));
    save(&mut session, "s1", &["a", "b"]);
    session.take_refetch_listing();

    server.gone(true);
    let next_page = Request::Call {
        method: "set.get".into(),
        params: json!({ "id": "s1", "cursor": "page-2" }),
    };
    let ids = [session.request(set_get("s1")), session.request(next_page)];
    let replies = wait_replies(&mut session, &ids);
    assert!(replies[0].result.is_ok(), "{:?}", replies[0]);
    // A later page is not answered from the copies: the copy is the whole listing, which
    // would be appended to the pages already shown.
    assert!(
        matches!(replies[1].result, Err(ClientError::Unreachable(_))),
        "{:?}",
        replies[1]
    );
    assert!(session.is_offline());
    assert!(!session.take_refetch_listing());

    server.gone(false);
    let id = session.request(set_get("s1"));
    assert!(wait_replies(&mut session, &[id])[0].result.is_ok());
    assert!(session.degraded().is_none());
    // What is on screen came from the copies: fetch it again.
    assert!(session.take_refetch_listing());
}

#[test]
fn a_known_library_with_nothing_saved_still_fails() {
    let data = tempfile::tempdir().unwrap();
    let server = FakeServer::start();
    let profile = profile(server.addr, "t");
    let session = open(&profile, data.path());
    assert!(matches!(session.state(), SessionState::Ready { .. }));
    drop(session);

    let session = open(&at(&profile, free_addr()), data.path());
    assert!(
        matches!(
            session.state(),
            SessionState::Failed(ClientError::Unreachable(_))
        ),
        "{:?}",
        session.state()
    );
    assert!(!session.is_offline());
}
