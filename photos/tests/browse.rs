//! The library screen against stub servers: paging, fetching again on a new epoch, and
//! watching a scan.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use asteria_framework_client::session::SessionState;
use asteria_photos::PhotosApp;
use asteria_photos::browse::SCANNING_POLL;
use corundum_framework::egui;
use serde_json::{Value, json};

use support::common::{StubResponse, photos_server_info, serve};
use support::*;

fn info(epoch: &str, scanning: bool, count: u64) -> Value {
    json!({
        "library_id": "lib",
        "owner": "owner",
        "epoch": epoch,
        "item_count": count,
        "scanning": scanning,
    })
}

/// A server whose `library.info` answers come from `info_for(n)` (n = 1 for the handshake's)
/// and whose `folder.list` answers come from `list(params, n)`. Returns the address and the
/// number of `library.info` calls so far.
fn server(
    info_for: impl Fn(u32) -> Value + Send + Sync + 'static,
    list: impl Fn(&Value, u32) -> Value + Send + Sync + 'static,
) -> (std::net::SocketAddr, Arc<Mutex<u32>>) {
    let infos = Arc::new(Mutex::new(0u32));
    let lists = Mutex::new(0u32);
    let seen = infos.clone();
    let addr = serve(move |req| {
        let Some((id, method)) = req.rpc() else {
            return StubResponse::new(404, Vec::new());
        };
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" => {
                let mut n = seen.lock().unwrap();
                *n += 1;
                StubResponse::rpc_result(id, info_for(*n))
            }
            "folder.list" => {
                let mut n = lists.lock().unwrap();
                *n += 1;
                StubResponse::rpc_result(id, list(&params(req), *n))
            }
            _ => StubResponse::new(404, Vec::new()),
        }
    });
    (addr, infos)
}

fn start(addr: std::net::SocketAddr) -> (Fixture, egui::Context, PhotosApp) {
    let (fx, _) = fixture(&[("A", addr)], 0);
    let ctx = egui::Context::default();
    let app = PhotosApp::new(fx.paths.clone(), &ctx);
    (fx, ctx, app)
}

#[test]
fn the_next_page_is_fetched_when_the_grid_nears_the_end() {
    let cursors = Arc::new(Mutex::new(Vec::<Value>::new()));
    let log = cursors.clone();
    let (addr, _) = server(
        |_| info("e1", false, 4),
        move |p, _| {
            log.lock().unwrap().push(p["cursor"].clone());
            match p["cursor"].as_str() {
                None => json!({
                    "folders": ["sub"],
                    "items": [item("p1-0"), item("p1-1"), item("p1-2")],
                    "next_cursor": "c1",
                }),
                Some(_) => json!({
                    "folders": [],
                    "items": [item("p2-0")],
                    "next_cursor": null,
                }),
            }
        },
    );
    let (_fx, ctx, mut app) = start(addr);
    run_until(
        &ctx,
        &mut app,
        "both pages",
        |_| {},
        |app| item_ids(app).len() == 4,
    );
    assert_eq!(item_ids(&app), ["p1-0", "p1-1", "p1-2", "p2-0"]);
    let browse = app.connection().unwrap().browse();
    // Folders come with the first page only, and stay.
    assert_eq!(browse.folders(), ["sub"]);
    assert!(!browse.has_more());
    assert_eq!(*cursors.lock().unwrap(), [Value::Null, json!("c1")]);
}

#[test]
fn a_new_epoch_fetches_the_folder_again() {
    let (addr, _) = server(
        |n| info(if n == 1 { "e1" } else { "e2" }, false, 1),
        |_, n| json!({"folders": [], "items": [item(&format!("v{n}"))], "next_cursor": null}),
    );
    let (_fx, ctx, mut app) = start(addr);
    run_until(
        &ctx,
        &mut app,
        "the first listing",
        |_| {},
        |app| item_ids(app) == ["v1"],
    );
    app.refresh_library();
    // The old listing stays on screen until the new one is there.
    run_until(
        &ctx,
        &mut app,
        "the listing fetched again",
        |app| assert_eq!(item_ids(app).len(), 1),
        |app| item_ids(app) == ["v2"],
    );
}

#[test]
fn a_scan_is_watched_until_it_ends() {
    // Scanning for the handshake and the next answer, then done. The epoch stays the same,
    // as the real server's does (it moves only when the server rebuilds its cache).
    let (addr, infos) = server(
        |n| match n {
            1 | 2 => info("e1", true, u64::from(n) * 10),
            _ => info("e1", false, 30),
        },
        |_, n| json!({"folders": [], "items": [item(&format!("v{n}"))], "next_cursor": null}),
    );
    let (_fx, ctx, mut app) = start(addr);
    let started = Instant::now();
    let library = |app: &PhotosApp| match app.connection().map(|c| c.session().state()) {
        Some(SessionState::Ready { library }) => Some(library.clone()),
        _ => None,
    };
    run_until(
        &ctx,
        &mut app,
        "the scan to end",
        |_| {},
        |app| library(app).is_some_and(|l| !l.scanning),
    );
    // Asked again on its own, at the scanning interval, not sooner.
    assert!(*infos.lock().unwrap() >= 3);
    assert!(started.elapsed() >= SCANNING_POLL * 2 - SCANNING_POLL / 4);
    assert_eq!(library(&app).unwrap().item_count, 30);
    // The scan's end alone fetches the folder again, keeping the old listing meanwhile.
    run_until(
        &ctx,
        &mut app,
        "the listing after the scan",
        |app| assert_eq!(item_ids(app).len(), 1),
        |app| item_ids(app) == ["v2"],
    );
}

#[test]
fn a_new_item_count_between_scans_fetches_the_folder_again() {
    // Not scanning, one epoch throughout: the second answer has one item more (the watcher
    // found a new file), the third the same count again.
    let (addr, infos) = server(
        |n| info("e1", false, if n == 1 { 1 } else { 2 }),
        |_, n| json!({"folders": [], "items": [item(&format!("v{n}"))], "next_cursor": null}),
    );
    let (_fx, ctx, mut app) = start(addr);
    run_until(
        &ctx,
        &mut app,
        "the first listing",
        |_| {},
        |app| item_ids(app) == ["v1"],
    );
    app.refresh_library();
    run_until(
        &ctx,
        &mut app,
        "the listing fetched again",
        |app| assert_eq!(item_ids(app).len(), 1),
        |app| item_ids(app) == ["v2"],
    );
    // The same count again: nothing is fetched.
    app.refresh_library();
    run_until(
        &ctx,
        &mut app,
        "the third answer",
        |_| {},
        |_| *infos.lock().unwrap() == 3,
    );
    let start = Instant::now();
    while start.elapsed() < std::time::Duration::from_millis(300) {
        frame(&ctx, &mut app);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(item_ids(&app), ["v2"]);
}
