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
    let log: Log = Arc::default();
    let handler_log = log.clone();
    let addr = serve(move |req: &StubRequest| {
        if let Some((id, method)) = req.rpc() {
            let result = match method.as_str() {
                "server.info" => photos_server_info(),
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
