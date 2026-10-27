//! The upload engine on a `Session`, against a real server on a temporary library (and a stub
//! that never answers).

mod common;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asteria_framework::{Library, MediaKind};
use asteria_framework_api::codes;
use asteria_framework_api::{UPLOAD_ABORT, UPLOAD_STATUS, UploadSessionParams, UploadStatus};
use asteria_framework_client::profiles::{ServerProfile, Token};
use asteria_framework_client::rpc::{ClientError, RpcClient};
use asteria_framework_client::session::{Session, SessionDirs, SessionState};
use asteria_framework_client::upload::queue::{GroupState, QueuedGroup, UploadQueue, queue_path};
use asteria_framework_client::upload::{UploadSnapshot, UploadsState, set_retry_backoff};
use asteria_framework_server::upload::UploadStore;
use asteria_framework_server::{
    AppState, Cache, MediaFilter, Processor, ServerConfig, TokenEntry, Tokens, hash_pending,
    new_token, router, scan, spawn_writer, token_hash,
};
use axum::http::{Method, StatusCode};
use corundum_framework::egui;

use common::{
    DEADLINE, StubResponse, library_info, photos_client, photos_server_info, profile, serve,
    wait_settled,
};

const CHUNK: usize = 8 * 1024 * 1024;

struct Photos;
impl MediaFilter for Photos {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

/// What the server saw of `PUT /upload/...`, and a gate that holds chunks back.
#[derive(Default)]
struct Puts {
    /// Chunk requests that reached the server so far.
    arrived: AtomicUsize,
    /// The index of each chunk stored (204), in order.
    stored: Mutex<Vec<u64>>,
    /// The chunk requests from this one on (counting from 0) wait until `released`.
    hold_from: AtomicUsize,
    released: AtomicBool,
    /// The RPC methods called, in order.
    methods: Mutex<Vec<String>>,
    /// This many `upload.commit` answers are lost (a 503 goes back instead) after the commit
    /// has run.
    lose_commits: AtomicUsize,
}

impl Puts {
    fn new() -> Puts {
        Puts {
            hold_from: AtomicUsize::new(usize::MAX),
            ..Puts::default()
        }
    }

    fn stored(&self) -> Vec<u64> {
        self.stored.lock().unwrap().clone()
    }

    fn methods(&self) -> Vec<String> {
        self.methods.lock().unwrap().clone()
    }
}

/// A photos server on a temporary library holding `a.jpg` (hashed), listening on a free local
/// port. Stops when dropped.
struct TestServer {
    dir: tempfile::TempDir,
    rt: Option<tokio::runtime::Runtime>,
    addr: SocketAddr,
    token: String,
    library_id: String,
    puts: Arc<Puts>,
    /// One profile for the whole test, so every session on it uses the same queue.
    profile: ServerProfile,
}

impl TestServer {
    fn start() -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let library_id = lib.config().id.0.to_string();
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        fs::write(root.join("a.jpg"), fixture("red.jpg")).unwrap();
        scan(&lib, &cache, &Photos).unwrap();
        hash_pending(&lib, &cache, &AtomicBool::new(false)).unwrap();
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
        let uploads = Arc::new(UploadStore::open(lib.root()).unwrap());
        let puts = Arc::new(Puts::new());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let hooks = puts.clone();
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
                uploads,
            );
            let app = router(state).layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let hooks = hooks.clone();
                    async move { count_puts(&hooks, req, next).await }
                },
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _rescan_rx = rescan_rx;
                axum::serve(listener, app).await.unwrap();
            });
            addr
        });
        let profile = profile(addr, &token);
        TestServer {
            dir,
            rt: Some(rt),
            addr,
            token,
            library_id,
            puts,
            profile,
        }
    }

    fn profile(&self) -> ServerProfile {
        self.profile.clone()
    }

    /// The queue file of this server's profile under `data`.
    fn queue_file(&self, data: &Path) -> PathBuf {
        queue_path(data, &self.library_id, &self.profile.id.to_string()).unwrap()
    }

    fn inbox(&self, name: &str) -> PathBuf {
        self.dir.path().join("lib/Inbox").join(name)
    }

    /// `upload.status` of `session`, asked directly.
    fn status(&self, session: &str) -> Result<UploadStatus, ClientError> {
        let url = url::Url::parse(&format!("http://{}", self.addr)).unwrap();
        let rpc = RpcClient::new(&url, Token::new(&self.token)).unwrap();
        rpc.call(
            UPLOAD_STATUS,
            &UploadSessionParams {
                session: session.into(),
            },
        )
    }
}

impl TestServer {
    /// `upload.abort` of `session`, asked directly (as if it had expired).
    fn abort(&self, session: &str) {
        let url = url::Url::parse(&format!("http://{}", self.addr)).unwrap();
        let rpc = RpcClient::new(&url, Token::new(&self.token)).unwrap();
        rpc.call::<_, serde_json::Value>(
            UPLOAD_ABORT,
            &UploadSessionParams {
                session: session.into(),
            },
        )
        .unwrap();
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

async fn count_puts(
    puts: &Puts,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let chunk = if req.method() == Method::PUT && req.uri().path().starts_with("/upload/") {
        req.uri()
            .path()
            .rsplit('/')
            .next()
            .and_then(|c| c.parse::<u64>().ok())
    } else {
        None
    };
    let mut commit = false;
    let req = if req.method() == Method::POST && req.uri().path() == "/rpc" {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let call: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let method = call["method"].as_str().unwrap_or_default().to_string();
        commit = method == "upload.commit";
        puts.methods.lock().unwrap().push(method);
        axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes))
    } else {
        req
    };
    if chunk.is_some() {
        let n = puts.arrived.fetch_add(1, Ordering::SeqCst);
        while n >= puts.hold_from.load(Ordering::SeqCst) && !puts.released.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let resp = next.run(req).await;
    if commit
        && puts
            .lose_commits
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    {
        // The commit ran, but its answer never reaches the client.
        return axum::response::Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("retry-after", "0")
            .body(axum::body::Body::empty())
            .unwrap();
    }
    if let Some(c) = chunk
        && resp.status() == StatusCode::NO_CONTENT
    {
        puts.stored.lock().unwrap().push(c);
    }
    resp
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../asteria-framework/tests/fixtures")
            .join(name),
    )
    .unwrap()
}

/// `name` from the fixtures, padded to `len` bytes (decoders ignore what follows the image).
fn padded(name: &str, len: usize) -> Vec<u8> {
    let mut bytes = fixture(name);
    let start = bytes.len();
    bytes.extend((start..len).map(|i| (i % 251) as u8));
    bytes
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

/// Short back-off waits, so a test does not sit through the real ones.
fn quick_backoff() {
    set_retry_backoff(Duration::from_millis(50), Duration::from_millis(400));
}

fn open(profile: &ServerProfile, data: &Path) -> Session {
    quick_backoff();
    let mut session = Session::open_with_dirs(
        profile,
        photos_client(),
        egui::Context::default(),
        SessionDirs {
            cache: None,
            data: Some(data.to_path_buf()),
        },
    );
    wait_settled(&mut session);
    assert!(
        matches!(session.state(), SessionState::Ready { .. }),
        "{:?}",
        session.state()
    );
    session
}

fn describe(snap: &UploadSnapshot) -> String {
    let states: Vec<_> = snap.groups.iter().take(5).map(|g| &g.state).collect();
    format!("{:?} {:?} {states:?}", snap.state, snap.summary)
}

/// Polls `session` until its uploads satisfy `done`.
fn wait_for(
    session: &mut Session,
    what: &str,
    done: impl Fn(&UploadSnapshot) -> bool,
) -> UploadSnapshot {
    let start = Instant::now();
    loop {
        session.poll();
        let snap = session.uploads().expect("uploads").snapshot();
        if done(&snap) {
            return snap;
        }
        assert!(start.elapsed() < DEADLINE, "{what}: {}", describe(&snap));
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits (polling `session`) until `cond` holds.
fn wait_until(session: &mut Session, what: &str, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        session.poll();
        assert!(start.elapsed() < DEADLINE, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_plain(what: &str, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < DEADLINE, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn group<'a>(snap: &'a UploadSnapshot, inbox_path: &str) -> &'a QueuedGroup {
    snap.groups
        .iter()
        .find(|g| g.files[0].inbox_path == inbox_path)
        .unwrap_or_else(|| panic!("no group {inbox_path}: {}", describe(snap)))
}

fn is_done(g: &QueuedGroup) -> bool {
    matches!(g.state, GroupState::Done { .. })
}

#[test]
fn drop_to_commit() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let big = padded("blue.jpg", CHUNK + 1000);
    let files = [
        ("big.jpg", big),
        ("red.png", fixture("red.png")),
        ("blue.webp", fixture("blue.webp")),
    ];
    let paths = files.iter().map(|(n, b)| write(src.path(), n, b)).collect();
    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(paths);
    let snap = wait_for(&mut session, "all done", |s| {
        s.groups.len() == 3 && s.groups.iter().all(is_done)
    });
    assert_eq!(snap.summary.done, 3);
    assert_eq!(snap.excluded, 0);
    for (name, bytes) in &files {
        assert_eq!(&fs::read(server.inbox(name)).unwrap(), bytes, "{name}");
    }
    let g = group(&snap, "big.jpg");
    assert_eq!(
        g.state,
        GroupState::Done {
            placed: vec!["Inbox/big.jpg".into()],
            warnings: vec![],
        }
    );
    assert_eq!(g.sent, (CHUNK + 1000) as u64);
    assert_eq!(g.files[0].size, (CHUNK + 1000) as u64);
    assert!(g.files[0].mtime_ns > 0);
    // The big file went in two chunks, sent once each.
    let mut stored = server.puts.stored();
    stored.sort();
    assert_eq!(stored, vec![0, 0, 0, 1]);
}

#[test]
fn resumes_after_session_drop() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let bytes = padded("blue.jpg", 2 * CHUNK + 1000);
    let path = write(src.path(), "big.jpg", &bytes);
    server.puts.hold_from.store(1, Ordering::SeqCst);

    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    // The first chunk is stored and the second held on its way.
    wait_until(&mut session, "second chunk sent", || {
        server.puts.arrived.load(Ordering::SeqCst) == 2
    });
    assert_eq!(server.puts.stored(), vec![0]);
    drop(session);
    // The chunk in flight when the session went away still lands.
    server.puts.released.store(true, Ordering::SeqCst);
    wait_plain("second chunk stored", || server.puts.stored().len() == 2);
    // The new session waits for the old engine to save and let go of the queue.

    let mut session = open(&server.profile(), data.path());
    wait_for(&mut session, "done after resuming", |s| {
        s.groups.len() == 1 && is_done(&s.groups[0])
    });
    // Only the missing chunk was sent again.
    assert_eq!(server.puts.stored(), vec![0, 1, 2]);
    assert_eq!(fs::read(server.inbox("big.jpg")).unwrap(), bytes);
}

#[test]
fn changed_local_file_restarts() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write(src.path(), "x.jpg", &padded("blue.jpg", CHUNK + 1000));
    server.puts.hold_from.store(1, Ordering::SeqCst);

    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(vec![path.clone()]);
    wait_until(&mut session, "first chunk stored", || {
        server.puts.stored().len() == 1
    });
    let new = fixture("blue.webp");
    fs::write(&path, &new).unwrap();
    server.puts.released.store(true, Ordering::SeqCst);

    let snap = wait_for(&mut session, "done", |s| {
        s.groups.len() == 1 && is_done(&s.groups[0])
    });
    assert_eq!(fs::read(server.inbox("x.jpg")).unwrap(), new);
    assert!(!server.inbox("x (2).jpg").exists());
    assert_eq!(snap.groups[0].files[0].size, new.len() as u64);
}

#[test]
fn duplicate_waits_while_others_go() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    // `red.jpg` is the server's `a.jpg`.
    let red = write(src.path(), "red.jpg", &fixture("red.jpg"));
    let blue = write(src.path(), "blue.jpg", &fixture("blue.jpg"));

    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(vec![red, blue]);
    let snap = wait_for(&mut session, "blue done, red waiting", |s| {
        s.groups.len() == 2
            && is_done(group(s, "blue.jpg"))
            && matches!(
                group(s, "red.jpg").state,
                GroupState::AwaitingDecision { .. }
            )
    });
    let red_group = group(&snap, "red.jpg");
    assert_eq!(
        red_group.state,
        GroupState::AwaitingDecision {
            existing: vec!["a.jpg".into()]
        }
    );
    assert_eq!(snap.summary.awaiting, 1);
    assert!(server.inbox("blue.jpg").exists());
    assert!(!server.inbox("red.jpg").exists());

    assert!(session.uploads().unwrap().decide(red_group.id, true));
    wait_for(&mut session, "red done", |s| is_done(group(s, "red.jpg")));
    assert_eq!(
        fs::read(server.inbox("red.jpg")).unwrap(),
        fixture("red.jpg")
    );
}

#[test]
fn discard_aborts_session() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write(src.path(), "big.jpg", &padded("blue.jpg", CHUNK + 1000));
    server.puts.hold_from.store(1, Ordering::SeqCst);

    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    wait_until(&mut session, "first chunk stored", || {
        server.puts.stored().len() == 1
    });
    let snap = session.uploads().unwrap().snapshot();
    let id = snap.groups[0].id;
    let upload = snap.groups[0].files[0].session.clone().expect("a session");
    assert!(server.status(&upload).is_ok());

    assert!(session.uploads().unwrap().cancel(id));
    server.puts.released.store(true, Ordering::SeqCst);
    wait_for(&mut session, "session dropped from the queue", |s| {
        s.groups[0].state == GroupState::Discarded && s.groups[0].files[0].session.is_none()
    });
    match server.status(&upload) {
        Err(ClientError::Rpc { code, .. }) => assert_eq!(code, codes::NOT_FOUND),
        other => panic!("expected NOT_FOUND, got {other:?}"),
    }
    assert!(!server.inbox("big.jpg").exists());
    // The queue on disk says the same.
    let file = server.queue_file(data.path());
    wait_plain("queue saved", || {
        UploadQueue::load(&file).is_ok_and(|q| {
            q.groups
                .first()
                .is_some_and(|g| g.state == GroupState::Discarded && g.files[0].session.is_none())
        })
    });
}

#[test]
fn unreachable_server_backs_off_without_busy_loop() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let addr = serve(move |req| match req.rpc() {
        Some((id, m)) if m == "server.info" => StubResponse::rpc_result(id, photos_server_info()),
        Some((id, m)) if m == "library.info" => {
            StubResponse::rpc_result(id, library_info("unreachable-lib"))
        }
        _ => {
            counter.fetch_add(1, Ordering::SeqCst);
            StubResponse::hang_up()
        }
    });
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write(src.path(), "blue.jpg", &fixture("blue.jpg"));
    let mut session = open(&profile(addr, "t"), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    wait_until(&mut session, "first attempt", || {
        attempts.load(Ordering::SeqCst) >= 1
    });
    let before = attempts.load(Ordering::SeqCst);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        session.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    // 50, 100, 200, 400, 400 ms...: a handful of tries, not thousands.
    let tries = attempts.load(Ordering::SeqCst) - before;
    assert!((2..=12).contains(&tries), "{tries} tries in 1.5 s");
    let snap = session.uploads().unwrap().snapshot();
    assert!(
        matches!(
            snap.state,
            UploadsState::Waiting {
                error: ClientError::Unreachable(_)
            }
        ),
        "{:?}",
        snap.state
    );
    assert_eq!(snap.groups[0].state, GroupState::Checking);
}

#[test]
fn switching_servers_pauses_queue() {
    let a = TestServer::start();
    let b = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let bytes = padded("blue.jpg", CHUNK + 1000);
    let path = write(src.path(), "big.jpg", &bytes);
    a.puts.hold_from.store(0, Ordering::SeqCst);

    let mut on_a = open(&a.profile(), data.path());
    on_a.uploads().unwrap().drop_paths(vec![path]);
    wait_until(&mut on_a, "a chunk reached A", || {
        a.puts.arrived.load(Ordering::SeqCst) >= 1
    });
    drop(on_a);

    let mut on_b = open(&b.profile(), data.path());
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        on_b.poll();
        assert!(on_b.uploads().unwrap().snapshot().groups.is_empty());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(b.puts.arrived.load(Ordering::SeqCst), 0);
    drop(on_b);

    // A's queue is still on disk, unfinished.
    let file = a.queue_file(data.path());
    wait_plain("A's queue saved with its session", || {
        UploadQueue::load(&file).is_ok_and(|q| {
            q.groups.len() == 1
                && matches!(q.groups[0].state, GroupState::Uploading { .. })
                && q.groups[0].files[0].session.is_some()
        })
    });

    // Back on A, it carries on.
    a.puts.released.store(true, Ordering::SeqCst);
    wait_plain("held chunk stored", || a.puts.stored().len() == 1);
    let mut on_a = open(&a.profile(), data.path());
    wait_for(&mut on_a, "done on A", |s| {
        s.groups.len() == 1 && is_done(&s.groups[0])
    });
    assert_eq!(a.puts.stored(), vec![0, 1]);
    assert_eq!(fs::read(a.inbox("big.jpg")).unwrap(), bytes);
    assert!(!b.inbox("big.jpg").exists());
}

#[test]
fn ten_thousand_files_do_not_block_poll() {
    const N: usize = 10_000;
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let many = src.path().join("many");
    fs::create_dir(&many).unwrap();
    // All the server's `a.jpg`, so each one stops at the duplicate check.
    let red = fixture("red.jpg");
    for i in 0..N {
        fs::write(many.join(format!("f{i:05}.jpg")), &red).unwrap();
    }
    let mut session = open(&server.profile(), data.path());
    let limit = Duration::from_millis(50);
    let start = Instant::now();
    session.uploads().unwrap().drop_paths(vec![many]);
    assert!(
        start.elapsed() < limit,
        "drop_paths took {:?}",
        start.elapsed()
    );
    let start = Instant::now();
    loop {
        let t = Instant::now();
        session.poll();
        let snap = session.uploads().unwrap().snapshot();
        let took = t.elapsed();
        assert!(took < limit, "poll and snapshot took {took:?}");
        if snap.summary.awaiting == N {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "{}",
            describe(&snap)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cancel_reaches_disk_while_a_request_hangs() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write(src.path(), "big.jpg", &padded("blue.jpg", CHUNK + 1000));
    server.puts.hold_from.store(1, Ordering::SeqCst);

    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    wait_until(&mut session, "second chunk held", || {
        server.puts.arrived.load(Ordering::SeqCst) == 2
    });
    let id = session.uploads().unwrap().snapshot().groups[0].id;
    assert!(session.uploads().unwrap().cancel(id));
    drop(session);
    // The engine is still stuck in the PUT; the cancel is on disk regardless.
    let file = server.queue_file(data.path());
    wait_plain("cancel saved", || {
        UploadQueue::load(&file).is_ok_and(|q| {
            q.groups
                .first()
                .is_some_and(|g| g.state == GroupState::Discarded)
        })
    });

    // Once the PUT returns, the next session aborts the session and sends nothing more.
    server.puts.released.store(true, Ordering::SeqCst);
    let mut session = open(&server.profile(), data.path());
    let snap = wait_for(&mut session, "session aborted", |s| {
        s.groups.len() == 1 && s.groups[0].files[0].session.is_none()
    });
    assert_eq!(snap.groups[0].state, GroupState::Discarded);
    assert_eq!(server.puts.stored(), vec![0, 1]);
    assert!(!server.inbox("big.jpg").exists());
}

#[test]
fn unauthorized_stops_the_engine() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let addr = serve(move |req| match req.rpc() {
        Some((id, m)) if m == "server.info" => StubResponse::rpc_result(id, photos_server_info()),
        Some((id, m)) if m == "library.info" => {
            StubResponse::rpc_result(id, library_info("unauthorized-lib"))
        }
        _ => {
            counter.fetch_add(1, Ordering::SeqCst);
            StubResponse::new(401, Vec::new())
        }
    });
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write(src.path(), "blue.jpg", &fixture("blue.jpg"));
    let mut session = open(&profile(addr, "t"), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    let snap = wait_for(&mut session, "stopped", |s| {
        matches!(
            s.state,
            UploadsState::Stopped {
                error: ClientError::Unauthorized
            }
        )
    });
    assert_eq!(snap.groups[0].state, GroupState::Checking);
    // Stopped means no more tries, unlike a back-off (50 ms here).
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        session.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn expired_session_begins_again() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let bytes = padded("blue.jpg", CHUNK + 1000);
    let path = write(src.path(), "big.jpg", &bytes);
    server.puts.hold_from.store(0, Ordering::SeqCst);

    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    wait_until(&mut session, "first chunk held", || {
        server.puts.arrived.load(Ordering::SeqCst) == 1
    });
    let snap = wait_for(&mut session, "session known", |s| {
        s.groups[0].files[0].session.is_some()
    });
    let first = snap.groups[0].files[0].session.clone().unwrap();
    // The session disappears on the server while its chunk is on the way.
    server.abort(&first);
    server.puts.released.store(true, Ordering::SeqCst);
    wait_for(&mut session, "done", |s| is_done(&s.groups[0]));
    assert_eq!(fs::read(server.inbox("big.jpg")).unwrap(), bytes);
    // The held chunk found no session; both chunks went to the new one.
    assert_eq!(server.puts.stored(), vec![0, 1]);
}

#[test]
fn two_profiles_on_one_server_keep_separate_queues() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write(src.path(), "big.jpg", &padded("blue.jpg", CHUNK + 1000));
    server.puts.hold_from.store(0, Ordering::SeqCst);

    let mut first = open(&server.profile(), data.path());
    first.uploads().unwrap().drop_paths(vec![path]);
    wait_until(&mut first, "a chunk on its way", || {
        server.puts.arrived.load(Ordering::SeqCst) >= 1
    });
    drop(first);

    // Another profile (another user, say) on the same server sees none of it.
    let other = profile(server.addr, &server.token);
    let mut second = open(&other, data.path());
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        second.poll();
        assert!(second.uploads().unwrap().snapshot().groups.is_empty());
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(second);
    let file = server.queue_file(data.path());
    wait_plain("the first profile's queue is kept", || {
        UploadQueue::load(&file).is_ok_and(|q| q.groups.len() == 1)
    });
    server.puts.released.store(true, Ordering::SeqCst);
}

#[test]
fn a_lost_commit_answer_is_asked_again_not_sent_again() {
    let server = TestServer::start();
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let bytes = padded("blue.jpg", CHUNK + 1000);
    let path = write(src.path(), "big.jpg", &bytes);
    server.puts.lose_commits.store(1, Ordering::SeqCst);

    let mut session = open(&server.profile(), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    let snap = wait_for(&mut session, "done", |s| {
        s.groups.len() == 1 && is_done(&s.groups[0])
    });
    assert_eq!(
        snap.groups[0].state,
        GroupState::Done {
            placed: vec!["Inbox/big.jpg".into()],
            warnings: vec![],
        }
    );
    // Placed once, sent once.
    assert_eq!(fs::read(server.inbox("big.jpg")).unwrap(), bytes);
    assert!(!server.inbox("big (2).jpg").exists());
    assert_eq!(server.puts.stored(), vec![0, 1]);
    // After the lost answer, the commit is simply asked again.
    let methods: Vec<String> = server
        .puts
        .methods()
        .into_iter()
        .filter(|m| m.starts_with("upload."))
        .collect();
    let first = methods.iter().position(|m| m == "upload.commit").unwrap();
    assert_eq!(
        methods[first..],
        ["upload.commit", "upload.commit"],
        "{methods:?}"
    );
}

#[test]
fn a_server_without_uploads_stops_the_engine() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let addr = serve(move |req| match req.rpc() {
        Some((id, m)) if m == "server.info" => StubResponse::rpc_result(id, photos_server_info()),
        Some((id, m)) if m == "library.info" => {
            StubResponse::rpc_result(id, library_info("old-server-lib"))
        }
        Some((id, _)) => {
            counter.fetch_add(1, Ordering::SeqCst);
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": codes::METHOD_NOT_FOUND, "message": "method not found"},
            });
            StubResponse::new(200, body.to_string().into_bytes())
                .header("content-type", "application/json")
        }
        None => StubResponse::new(404, Vec::new()),
    });
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let path = write(src.path(), "blue.jpg", &fixture("blue.jpg"));
    let mut session = open(&profile(addr, "t"), data.path());
    session.uploads().unwrap().drop_paths(vec![path]);
    let snap = wait_for(&mut session, "stopped", |s| {
        matches!(s.state, UploadsState::Stopped { .. })
    });
    assert!(snap.state.server_lacks_uploads(), "{:?}", snap.state);
    // The group is not failed: it waits for a server that takes uploads.
    assert_eq!(snap.groups[0].state, GroupState::Checking);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        session.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}
