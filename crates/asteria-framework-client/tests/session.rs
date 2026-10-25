//! `Session` against a real server on a temporary library, and against a stub that answers
//! slowly.

mod common;

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use asteria_framework::{Library, MediaKind};
use asteria_framework_api::{ClientInfo, FolderListParams, HandshakeError};
use asteria_framework_client::profiles::ServerProfile;
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::{
    Reply, Request, RequestId, Response, Session, SessionState,
};
use asteria_framework_server::{
    AppState, Cache, MediaFilter, Processor, ServerConfig, TokenEntry, Tokens, new_token, router,
    scan, spawn_writer, token_hash,
};
use corundum_framework::egui;
use serde_json::json;

use common::{
    DEADLINE, StubRequest, StubResponse, library_info, photos_client, photos_server_info, profile,
    serve, wait_settled,
};

struct Photos;
impl MediaFilter for Photos {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

/// A photos server on a temporary library, listening on a free local port. Stops when dropped.
struct TestServer {
    _dir: tempfile::TempDir,
    rt: Option<tokio::runtime::Runtime>,
    addr: SocketAddr,
    token: String,
    library_id: String,
}

impl TestServer {
    fn start() -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        std::fs::create_dir(&root).unwrap();
        let lib = Arc::new(Library::init(&root, "owner").unwrap());
        let library_id = lib.config().id.0.to_string();
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let red = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../asteria-framework/tests/fixtures/red.jpg");
        std::fs::copy(&red, root.join("a.jpg")).unwrap();
        scan(&lib, &cache, &Photos).unwrap();
        let token = new_token();
        let cfg = ServerConfig {
            library: root,
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            token: vec![TokenEntry {
                user: lib.owner(),
                hash: token_hash(&token),
            }],
        };
        let processor = Arc::new(Processor::new(
            dir.path().join("cache/thumbs"),
            1,
            Duration::from_secs(60),
        ));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let addr = rt.block_on(async {
            let (writer, _h) = spawn_writer(lib.clone(), cache.clone());
            let (rescan, rescan_rx) = tokio::sync::mpsc::channel(1);
            let state = AppState::new(
                lib,
                cache,
                writer,
                Arc::new(Tokens::from_config(&cfg)),
                asteria_photos_api::APP,
                asteria_photos_api::APP_API,
                Arc::new(AtomicBool::new(false)),
                rescan,
                Arc::new(Photos),
                processor,
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _rescan_rx = rescan_rx;
                axum::serve(listener, router(state)).await.unwrap();
            });
            addr
        });
        TestServer {
            _dir: dir,
            rt: Some(rt),
            addr,
            token,
            library_id,
        }
    }

    fn profile(&self, token: &str) -> ServerProfile {
        profile(self.addr, token)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

#[test]
fn handshake_ok_reports_library_id() {
    let server = TestServer::start();
    let mut session = Session::open(
        &server.profile(&server.token),
        photos_client(),
        egui::Context::default(),
    );
    assert!(matches!(session.state(), SessionState::Connecting));
    wait_settled(&mut session);
    match session.state() {
        SessionState::Ready { library } => {
            assert_eq!(library.library_id, server.library_id);
            assert_eq!(library.item_count, 1);
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn handshake_opens_the_library_cache() {
    let server = TestServer::start();
    let root = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let profile = server.profile(&server.token);
    let mut session = Session::open_with_cache(
        &profile,
        photos_client(),
        ctx.clone(),
        Some(root.path().to_path_buf()),
    );
    wait_settled(&mut session);
    let cache = session
        .media_cache()
        .expect("cache opened by the handshake");
    let hash = format!("blake3:{}", "a".repeat(64));
    let stored = cache.put(Some(&hash), "thumb", b"x").unwrap().unwrap();
    assert!(stored.starts_with(root.path().join(&server.library_id)));
    // Without a cache root there is no cache.
    let mut plain = Session::open(&profile, photos_client(), ctx);
    wait_settled(&mut plain);
    assert!(plain.media_cache().is_none());
}

#[test]
fn wrong_token_is_unauthorized() {
    let server = TestServer::start();
    let mut session = Session::open(
        &server.profile("not-the-token"),
        photos_client(),
        egui::Context::default(),
    );
    wait_settled(&mut session);
    assert!(
        matches!(
            session.state(),
            SessionState::Failed(ClientError::Unauthorized)
        ),
        "{:?}",
        session.state()
    );
}

#[test]
fn app_mismatch_is_handshake_error() {
    let server = TestServer::start();
    let music = ClientInfo {
        app: asteria_music_api::APP.into(),
        app_api: asteria_music_api::APP_API,
    };
    let mut session = Session::open(
        &server.profile(&server.token),
        music,
        egui::Context::default(),
    );
    wait_settled(&mut session);
    match session.state() {
        SessionState::Failed(ClientError::Handshake(HandshakeError::AppMismatch {
            client,
            server,
        })) => {
            assert_eq!(client, "asteria-music");
            assert_eq!(server, "asteria-photos");
        }
        other => panic!("expected an app mismatch, got {other:?}"),
    }
}

#[test]
fn unreachable_server_fails_without_hanging() {
    // A port that was free a moment ago and has nothing listening now.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let start = Instant::now();
    let mut session = Session::open(
        &profile(addr, "whatever"),
        photos_client(),
        egui::Context::default(),
    );
    wait_settled(&mut session);
    assert!(
        matches!(
            session.state(),
            SessionState::Failed(ClientError::Unreachable(_))
        ),
        "{:?}",
        session.state()
    );
    assert!(
        start.elapsed() < asteria_framework_client::rpc::CONNECT_TIMEOUT + Duration::from_secs(2),
        "took {:?}",
        start.elapsed()
    );
}

// --- A stub server that holds `folder.list` until the test lets it go. ---

/// What the stub reports to the test.
enum StubEvent {
    /// A `folder.list` arrived and is being held.
    Holding,
    /// The held `folder.list` answer was written to the socket.
    Released,
}

struct Stub {
    addr: SocketAddr,
    events: mpsc::Receiver<StubEvent>,
    release: mpsc::Sender<()>,
    shared: Arc<StubShared>,
}

impl Stub {
    /// The methods received so far, in order.
    fn methods(&self) -> Vec<String> {
        self.shared.methods.lock().unwrap().clone()
    }
}

/// State the stub's connection threads share.
struct StubShared {
    /// Taken by the first `folder.list`, which waits on it.
    release: Mutex<Option<mpsc::Receiver<()>>>,
    methods: Mutex<Vec<String>>,
    /// Answer `library.info` with another library id after the first time.
    change_library: bool,
    library_infos: AtomicUsize,
}

/// Answers `server.info` (as asteria-photos) and `library.info` at once. The first
/// `folder.list` is held until `release` fires and is answered with a folder named
/// "from-held"; later ones are answered at once with a folder named "from-prompt". With
/// `change_library`, every `library.info` after the first names "other-library".
fn start_stub(change_library: bool) -> Stub {
    let (events_tx, events) = mpsc::channel();
    let events_tx = Mutex::new(events_tx);
    let (release, release_rx) = mpsc::channel::<()>();
    let shared = Arc::new(StubShared {
        release: Mutex::new(Some(release_rx)),
        methods: Mutex::new(Vec::new()),
        change_library,
        library_infos: AtomicUsize::new(0),
    });
    let handler_shared = shared.clone();
    let addr = serve(move |req: &StubRequest| {
        let shared = &handler_shared;
        let (id, method) = req.rpc().expect("the stub only answers /rpc");
        shared.methods.lock().unwrap().push(method.clone());
        let events_tx = events_tx.lock().unwrap().clone();
        let mut held = false;
        let result = match method.as_str() {
            "server.info" => photos_server_info(),
            "library.info" => library_info(
                if shared.change_library && shared.library_infos.fetch_add(1, Ordering::SeqCst) > 0
                {
                    "other-library"
                } else {
                    "stub-library"
                },
            ),
            "folder.list" => {
                let rx = shared.release.lock().unwrap().take();
                match rx {
                    Some(rx) => {
                        held = true;
                        events_tx.send(StubEvent::Holding).unwrap();
                        let _ = rx.recv();
                        json!({"folders": ["from-held"], "items": [], "next_cursor": null})
                    }
                    None => json!({"folders": ["from-prompt"], "items": [], "next_cursor": null}),
                }
            }
            other => panic!("stub got unexpected method {other}"),
        };
        let mut resp = StubResponse::rpc_result(id, result);
        if held {
            resp.after_write = Some(Box::new(move || {
                let _ = events_tx.send(StubEvent::Released);
            }));
        }
        resp
    });
    Stub {
        addr,
        events,
        release,
        shared,
    }
}

fn folders(reply: &Reply) -> Vec<String> {
    match &reply.result {
        Ok(Response::FolderList(listing)) => listing.folders.clone(),
        other => panic!("expected a folder listing, got {other:?}"),
    }
}

fn list_root() -> Request {
    Request::FolderList(FolderListParams {
        path: String::new(),
        cursor: None,
        limit: None,
    })
}

#[test]
fn results_after_drop_are_discarded() {
    let stub = start_stub(false);
    let ctx = egui::Context::default();

    let mut a = Session::open(&profile(stub.addr, "t"), photos_client(), ctx.clone());
    wait_settled(&mut a);
    assert!(
        matches!(a.state(), SessionState::Ready { .. }),
        "{:?}",
        a.state()
    );
    let a_generation = a.generation();
    let a_id = a.request(list_root());
    assert!(matches!(
        stub.events.recv_timeout(DEADLINE).unwrap(),
        StubEvent::Holding
    ));

    // Dropping does not wait for the held request.
    let start = Instant::now();
    drop(a);
    assert!(start.elapsed() < Duration::from_secs(1), "drop blocked");

    let mut b = Session::open(&profile(stub.addr, "t"), photos_client(), ctx);
    assert_ne!(b.generation(), a_generation);
    let mut seen = wait_settled(&mut b);
    assert!(
        matches!(b.state(), SessionState::Ready { .. }),
        "{:?}",
        b.state()
    );

    // Let A's answer go out, then have B ask the same thing: B's first request has the same
    // tag A's had, so a leak would be indistinguishable by tag alone.
    stub.release.send(()).unwrap();
    assert!(matches!(
        stub.events.recv_timeout(DEADLINE).unwrap(),
        StubEvent::Released
    ));
    let b_id = b.request(list_root());
    assert_eq!(a_id, b_id);
    let start = Instant::now();
    let mut answered = false;
    // Keep watching for a while after B's own answer, in case A's arrives late.
    while start.elapsed() < Duration::from_millis(500) || !answered {
        assert!(start.elapsed() < DEADLINE, "B's request was never answered");
        let replies = b.poll();
        answered |= replies.iter().any(|r| r.id == b_id);
        seen.extend(replies);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(folders(&seen[0]), vec!["from-prompt".to_string()]);
}

/// Polls until every one of `ids` is answered; returns the replies in the order of `ids`.
fn wait_replies(session: &mut Session, ids: &[RequestId]) -> Vec<Reply> {
    let start = Instant::now();
    let mut got: Vec<Reply> = Vec::new();
    while !ids.iter().all(|id| got.iter().any(|r| r.id == *id)) {
        assert!(start.elapsed() < DEADLINE, "no reply to all of {ids:?}");
        got.extend(session.poll());
        std::thread::sleep(Duration::from_millis(5));
    }
    ids.iter()
        .map(|id| got.iter().find(|r| r.id == *id).unwrap().clone())
        .collect()
}

#[test]
fn requests_made_while_connecting_do_not_pass_a_failed_handshake() {
    let stub = start_stub(false);
    let music = ClientInfo {
        app: asteria_music_api::APP.into(),
        app_api: asteria_music_api::APP_API,
    };
    let mut session = Session::open(&profile(stub.addr, "t"), music, egui::Context::default());
    // Made before the handshake's answer has been polled.
    let id = session.request(list_root());
    let reply = wait_replies(&mut session, &[id]).remove(0);
    assert!(
        matches!(
            reply.result,
            Err(ClientError::Handshake(HandshakeError::AppMismatch { .. }))
        ),
        "{reply:?}"
    );
    assert!(matches!(
        session.state(),
        SessionState::Failed(ClientError::Handshake(_))
    ));
    assert_eq!(stub.methods(), vec!["server.info".to_string()]);
}

#[test]
fn another_library_id_fails_the_session() {
    let stub = start_stub(true);
    let mut session = Session::open(
        &profile(stub.addr, "t"),
        photos_client(),
        egui::Context::default(),
    );
    wait_settled(&mut session);
    assert!(
        matches!(session.state(), SessionState::Ready { .. }),
        "{:?}",
        session.state()
    );
    let info = session.request(Request::LibraryInfo);
    // Queued behind the check that will fail.
    let list = session.request(list_root());
    let replies = wait_replies(&mut session, &[info, list]);
    assert_eq!(replies[0].result, Err(ClientError::LibraryChanged));
    assert_eq!(replies[1].result, Err(ClientError::LibraryChanged));
    assert_eq!(
        session.state(),
        &SessionState::Failed(ClientError::LibraryChanged)
    );
    assert!(!stub.methods().iter().any(|m| m == "folder.list"));
}
