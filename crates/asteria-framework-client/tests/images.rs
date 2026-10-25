//! `ImageLoader` against a stub server that serves real WebP bytes.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use asteria_framework_api::MediaItem;
use asteria_framework_client::images::{ImageState, Preset};
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::{Session, SessionState};
use corundum_framework::egui;
use serde_json::json;

use common::{
    DEADLINE, StubRequest, StubResponse, library_info, photos_client, photos_server_info, profile,
    serve, wait_settled,
};

/// A 4x3 lossless WebP.
fn webp() -> Vec<u8> {
    let rgba: Vec<u8> = (0..4 * 3)
        .flat_map(|i| [i as u8 * 20, 0, 255, 255])
        .collect();
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(&rgba, 4, 3, image::ExtendedColorType::Rgba8)
        .unwrap();
    out
}

fn item(n: usize) -> MediaItem {
    serde_json::from_value(json!({
        "id": format!("item-{n}"),
        "path": format!("{n}.jpg"),
        "kind": "image",
        "size": 1,
        "modified": "2026-01-01T00:00:00Z",
        "rating": null,
        "subjects": [],
        "hierarchical_subjects": [],
        "description": null,
        "label": null,
        "content_hash": null,
        "file_hash": format!("blake3:{:064x}", n),
        "read_only_reason": null,
    }))
    .unwrap()
}

/// A media request the stub received: when, and which path.
type Log = Arc<Mutex<Vec<(Instant, String)>>>;

/// Answers the handshake, and every `GET /media/...` with `media(path, nth)` where `nth`
/// counts media requests from 0. Returns the address and the log of media requests.
fn media_stub(
    media: impl Fn(&str, usize) -> StubResponse + Send + Sync + 'static,
) -> (SocketAddr, Log) {
    media_stub_holding_handshake(None, media)
}

/// Like [`media_stub`]; with `handshake`, `server.info` is answered only once it fires.
fn media_stub_holding_handshake(
    handshake: Option<mpsc::Receiver<()>>,
    media: impl Fn(&str, usize) -> StubResponse + Send + Sync + 'static,
) -> (SocketAddr, Log) {
    let log: Log = Arc::default();
    let handler_log = log.clone();
    let handshake = Mutex::new(handshake);
    let addr = serve(move |req: &StubRequest| {
        if let Some((id, method)) = req.rpc() {
            let result = match method.as_str() {
                "server.info" => {
                    if let Some(rx) = handshake.lock().unwrap().take() {
                        let _ = rx.recv_timeout(DEADLINE);
                    }
                    photos_server_info()
                }
                "library.info" => library_info("stub-library"),
                other => panic!("stub got unexpected method {other}"),
            };
            return StubResponse::rpc_result(id, result);
        }
        assert_eq!(req.method, "GET");
        let nth = {
            let mut log = handler_log.lock().unwrap();
            log.push((Instant::now(), req.path.clone()));
            log.len() - 1
        };
        media(&req.path, nth)
    });
    (addr, log)
}

fn webp_response() -> StubResponse {
    StubResponse::new(200, webp()).header("content-type", "image/webp")
}

fn paths(log: &Log) -> Vec<String> {
    log.lock().unwrap().iter().map(|(_, p)| p.clone()).collect()
}

fn ready_session(addr: SocketAddr, ctx: &egui::Context, cache_root: &Path) -> Session {
    let mut session = Session::open_with_cache(
        &profile(addr, "t"),
        photos_client(),
        ctx.clone(),
        Some(cache_root.to_path_buf()),
    );
    wait_settled(&mut session);
    assert!(
        matches!(session.state(), SessionState::Ready { .. }),
        "{:?}",
        session.state()
    );
    session
}

/// Runs one egui pass in which `f` runs.
fn frame<R>(ctx: &egui::Context, mut f: impl FnMut() -> R) -> R {
    let mut out = None;
    let mut output = ctx.run_pass(egui::RawInput::default(), |_| out = Some(f()));
    // There is no renderer to hand the texture uploads to.
    output.textures_delta.clear();
    out.unwrap()
}

/// Requests `item` once per frame until it is no longer loading.
fn wait_loaded(
    session: &mut Session,
    ctx: &egui::Context,
    item: &MediaItem,
    preset: Preset,
) -> ImageState {
    let start = Instant::now();
    loop {
        session.poll();
        let state = frame(ctx, || session.images().request(item, preset));
        if !matches!(state, ImageState::Loading) {
            return state;
        }
        assert!(start.elapsed() < DEADLINE, "image never loaded");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until `log` holds `n` entries.
fn wait_logged(log: &Log, n: usize) {
    let start = Instant::now();
    while log.lock().unwrap().len() < n {
        assert!(start.elapsed() < DEADLINE, "expected {n} media requests");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_ready(state: &ImageState) {
    match state {
        ImageState::Ready(texture) => assert_eq!(texture.size(), [4, 3]),
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn thumb_is_fetched_once_and_cached() {
    let (addr, log) = media_stub(|_, _| webp_response());
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let one = item(1);

    let mut a = ready_session(addr, &ctx, cache.path());
    // Two requests in one frame are one fetch.
    let (first, second) = frame(&ctx, || {
        (
            a.images().request(&one, Preset::Thumb),
            a.images().request(&one, Preset::Thumb),
        )
    });
    assert!(matches!(first, ImageState::Loading));
    assert!(matches!(second, ImageState::Loading));
    assert_ready(&wait_loaded(&mut a, &ctx, &one, Preset::Thumb));
    for _ in 0..3 {
        assert_ready(&frame(&ctx, || a.images().request(&one, Preset::Thumb)));
    }
    assert_eq!(paths(&log), vec!["/media/item-1/thumb".to_string()]);
    drop(a);

    // Another session on the same library reads the disk cache instead.
    let mut b = ready_session(addr, &ctx, cache.path());
    assert_ready(&wait_loaded(&mut b, &ctx, &one, Preset::Thumb));
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn retry_after_is_honoured() {
    let (addr, log) = media_stub(|_, nth| {
        if nth == 0 {
            StubResponse::new(503, Vec::new()).header("retry-after", "1")
        } else {
            webp_response()
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = ready_session(addr, &ctx, cache.path());
    assert_ready(&wait_loaded(&mut session, &ctx, &item(1), Preset::Thumb));
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2);
    let waited = log[1].0 - log[0].0;
    assert!(waited >= Duration::from_secs(1), "retried after {waited:?}");
}

#[test]
fn unprocessable_is_not_retried() {
    let (addr, log) = media_stub(|_, _| StubResponse::new(422, Vec::new()));
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = ready_session(addr, &ctx, cache.path());
    let broken = item(1);
    let state = wait_loaded(&mut session, &ctx, &broken, Preset::Thumb);
    assert!(
        matches!(state, ImageState::Failed(ClientError::Unprocessable)),
        "{state:?}"
    );
    // Keep asking for a while: the answer stays and nothing more is sent.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        session.poll();
        let state = frame(&ctx, || session.images().request(&broken, Preset::Thumb));
        assert!(matches!(
            state,
            ImageState::Failed(ClientError::Unprocessable)
        ));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn offscreen_requests_are_deprioritised() {
    // Every media request waits for a permit (or the deadline) before it is answered.
    let (permit, permits) = mpsc::channel::<()>();
    let permits = Mutex::new(permits);
    let (addr, log) = media_stub(move |_, _| {
        let _ = permits.lock().unwrap().recv_timeout(DEADLINE);
        webp_response()
    });
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = ready_session(addr, &ctx, cache.path());
    let items: Vec<MediaItem> = (0..10).map(item).collect();
    let thumb = |n: usize| format!("/media/item-{n}/thumb");

    // Frame 1: all ten are on screen. Six are fetched at once, in the order requested.
    frame(&ctx, || {
        for it in &items {
            session.images().request(it, Preset::Thumb);
        }
    });
    wait_logged(&log, 6);
    let mut first: Vec<String> = paths(&log);
    first.sort();
    assert_eq!(first, (0..6).map(thumb).collect::<Vec<_>>());

    // Frame 2: the view scrolled; only 8 and 9 are drawn. They go before 6 and 7.
    frame(&ctx, || {
        session.images().request(&items[8], Preset::Thumb);
        session.images().request(&items[9], Preset::Thumb);
    });
    permit.send(()).unwrap();
    wait_logged(&log, 7);
    assert_eq!(paths(&log)[6], thumb(8));
    permit.send(()).unwrap();
    wait_logged(&log, 8);
    assert_eq!(paths(&log)[7], thumb(9));
    for _ in 0..8 {
        permit.send(()).unwrap();
    }
    wait_logged(&log, 10);
    let mut rest = paths(&log)[8..].to_vec();
    rest.sort();
    assert_eq!(rest, vec![thumb(6), thumb(7)]);
    assert_eq!(log.lock().unwrap().len(), 10);
}

#[test]
fn preview_budget_evicts_the_least_recently_requested() {
    let (addr, log) = media_stub(|_, _| webp_response());
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = ready_session(addr, &ctx, cache.path());
    let items: Vec<MediaItem> = (0..9).map(item).collect();
    for it in &items {
        assert_ready(&wait_loaded(&mut session, &ctx, it, Preset::Preview));
    }
    // The ninth preview pushed the first one out; the latest stays.
    let state = frame(&ctx, || {
        session.images().request(&items[0], Preset::Preview)
    });
    assert!(matches!(state, ImageState::Loading), "{state:?}");
    assert_ready(&frame(&ctx, || {
        session.images().request(&items[8], Preset::Preview)
    }));
    // Asked again, it comes back from the disk cache.
    assert_ready(&wait_loaded(&mut session, &ctx, &items[0], Preset::Preview));
    assert_eq!(log.lock().unwrap().len(), 9);
}

#[test]
fn transient_failure_is_retried_after_backoff() {
    let (addr, log) = media_stub(|_, nth| {
        if nth == 0 {
            StubResponse::new(500, Vec::new())
        } else {
            webp_response()
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = ready_session(addr, &ctx, cache.path());
    session
        .images()
        .set_failure_backoff(Duration::from_millis(200), Duration::from_secs(1));
    let one = item(1);
    let state = wait_loaded(&mut session, &ctx, &one, Preset::Thumb);
    assert!(
        matches!(state, ImageState::Failed(ClientError::Protocol(_))),
        "{state:?}"
    );
    let failed_at = Instant::now();
    // Requests during the backoff keep the failure and send nothing.
    let state = frame(&ctx, || session.images().request(&one, Preset::Thumb));
    assert!(matches!(state, ImageState::Failed(_)), "{state:?}");
    assert_eq!(log.lock().unwrap().len(), 1);
    // After it, a request loads it again.
    let start = Instant::now();
    let state = loop {
        session.poll();
        let state = frame(&ctx, || session.images().request(&one, Preset::Thumb));
        if matches!(state, ImageState::Ready(_)) {
            break state;
        }
        assert!(start.elapsed() < DEADLINE, "never retried: {state:?}");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_ready(&state);
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2);
    assert!(log[1].0 >= failed_at + Duration::from_millis(150));
}

#[test]
fn queued_fetches_out_of_view_are_skipped() {
    // The first six media requests wait for a permit; later ones are answered at once.
    let (permit, permits) = mpsc::channel::<()>();
    let permits = Mutex::new(permits);
    let (addr, log) = media_stub(move |_, nth| {
        if nth < 6 {
            let _ = permits.lock().unwrap().recv_timeout(DEADLINE);
        }
        webp_response()
    });
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = ready_session(addr, &ctx, cache.path());
    let passed: Vec<MediaItem> = (0..50).map(item).collect();
    let landed = [item(100), item(101)];

    // Pass 1: a fast scroll shows fifty items; six start at once.
    frame(&ctx, || {
        for it in &passed {
            session.images().request(it, Preset::Thumb);
        }
    });
    wait_logged(&log, 6);
    // Passes 2 to 4: the view has settled on two other items.
    for _ in 0..3 {
        frame(&ctx, || {
            for it in &landed {
                session.images().request(it, Preset::Thumb);
            }
        });
    }
    for _ in 0..6 {
        permit.send(()).unwrap();
    }
    for it in &landed {
        assert_ready(&wait_loaded(&mut session, &ctx, it, Preset::Thumb));
    }
    // The other 44 of pass 1 are dropped as the pool reaches them, not fetched.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        session.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    let fetched = paths(&log);
    assert_eq!(fetched.len(), 8, "{fetched:?}");
    assert!(fetched.contains(&"/media/item-100/thumb".to_string()));
    assert!(fetched.contains(&"/media/item-101/thumb".to_string()));
    // Requested again, a skipped item is queued afresh.
    assert_ready(&wait_loaded(&mut session, &ctx, &passed[49], Preset::Thumb));
    assert_eq!(log.lock().unwrap().len(), 9);
}

#[test]
fn an_idle_ui_does_not_starve_its_requests() {
    let (permit, permits) = mpsc::channel::<()>();
    let permits = Mutex::new(permits);
    let (addr, log) = media_stub(move |_, nth| {
        if nth < 6 {
            let _ = permits.lock().unwrap().recv_timeout(DEADLINE);
        }
        webp_response()
    });
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = ready_session(addr, &ctx, cache.path());
    // One pass asks for ten; no pass follows. The four left in the queue are still fetched.
    frame(&ctx, || {
        for n in 0..10 {
            session.images().request(&item(n), Preset::Thumb);
        }
    });
    wait_logged(&log, 6);
    for _ in 0..6 {
        permit.send(()).unwrap();
    }
    wait_logged(&log, 10);
}

#[test]
fn requests_before_the_handshake_wait_for_it() {
    let (handshake, handshake_rx) = mpsc::channel::<()>();
    let (addr, log) = media_stub_holding_handshake(Some(handshake_rx), |_, _| webp_response());
    let cache = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    let mut session = Session::open_with_cache(
        &profile(addr, "t"),
        photos_client(),
        ctx.clone(),
        Some(cache.path().to_path_buf()),
    );
    let one = item(1);
    for _ in 0..3 {
        session.poll();
        let state = frame(&ctx, || session.images().request(&one, Preset::Thumb));
        assert!(matches!(state, ImageState::Loading), "{state:?}");
    }
    assert!(matches!(session.state(), SessionState::Connecting));
    assert!(log.lock().unwrap().is_empty());
    handshake.send(()).unwrap();
    assert_ready(&wait_loaded(&mut session, &ctx, &one, Preset::Thumb));
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn image_results_after_drop_are_discarded() {
    // The first media request (session A's) waits for `release_a`, and reports when its
    // answer is written; the second (session B's) waits for `release_b`.
    let (release_a, a_rx) = mpsc::channel::<()>();
    let (release_b, b_rx) = mpsc::channel::<()>();
    let (written, written_rx) = mpsc::channel::<()>();
    let held = Mutex::new((a_rx, b_rx, written));
    let (addr, log) = media_stub(move |_, nth| {
        let held = held.lock().unwrap();
        let rx = if nth == 0 { &held.0 } else { &held.1 };
        let _ = rx.recv_timeout(DEADLINE);
        let mut resp = webp_response();
        if nth == 0 {
            let written = held.2.clone();
            resp.after_write = Some(Box::new(move || {
                let _ = written.send(());
            }));
        }
        resp
    });
    let ctx = egui::Context::default();
    let one = item(1);
    let open = || {
        let mut s = Session::open(&profile(addr, "t"), photos_client(), ctx.clone());
        wait_settled(&mut s);
        s
    };

    let mut a = open();
    frame(&ctx, || a.images().request(&one, Preset::Thumb));
    wait_logged(&log, 1);
    drop(a);

    // B asks for the same image, so its first job has the same key and ticket as A's.
    let mut b = open();
    let state = frame(&ctx, || b.images().request(&one, Preset::Thumb));
    assert!(matches!(state, ImageState::Loading));
    wait_logged(&log, 2);
    release_a.send(()).unwrap();
    written_rx.recv_timeout(DEADLINE).unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        b.poll();
        let state = frame(&ctx, || b.images().request(&one, Preset::Thumb));
        assert!(
            matches!(state, ImageState::Loading),
            "A's image leaked: {state:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    release_b.send(()).unwrap();
    assert_ready(&wait_loaded(&mut b, &ctx, &one, Preset::Thumb));
}
