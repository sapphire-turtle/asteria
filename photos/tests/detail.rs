//! The detail pane's editing and the full-screen preview: against a real photos server on a
//! temporary library, and against stubs for the server's refusals.

mod support;

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asteria_framework_api::{ItemId, MediaItem, codes};
use asteria_photos::PhotosApp;
use asteria_photos::detail::{BUSY_RETRY_DELAY, Edit, EditError, Field, rating_after_click};
use corundum_framework::egui;
use serde_json::{Value, json};

use support::common::{StubResponse, library_info, photos_server_info, serve};
use support::server::TestServer;
use support::*;

/// The app on `server`, once its root lists `count` items.
fn open_real(server: &TestServer, count: usize) -> (Fixture, egui::Context, PhotosApp) {
    let (fx, _) = fixture_with_tokens(&[("real", server.addr, &server.token)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "the listing",
        |_| {},
        |app| item_ids(app).len() == count,
    );
    (fx, ctx, app)
}

fn listed<'a>(app: &'a PhotosApp, path: &str) -> &'a MediaItem {
    let items = app.connection().unwrap().browse().items();
    items.iter().find(|i| i.path == path).unwrap()
}

fn sending(app: &PhotosApp, field: Field) -> bool {
    app.detail().is_some_and(|d| d.is_sending(field))
}

#[test]
fn rating_edit_roundtrips() {
    let server = TestServer::start(&["a.jpg"], |_| {});
    let (_fx, ctx, mut app) = open_real(&server, 1);
    let id = listed(&app, "a.jpg").id.clone();
    app.select(&id);
    frame(&ctx, &mut app);
    let view = app.detail().expect("the selected item's detail");
    assert_eq!(view.item().id, id);
    assert!(view.editors_shown());
    assert_eq!(view.read_only_reason(), None);

    // The fourth star.
    assert!(app.edit(Edit::Rating(rating_after_click(None, 4))));
    // Its editor is disabled while the change is on its way; another click is not sent.
    assert!(sending(&app, Field::Rating));
    assert!(!sending(&app, Field::Label));
    assert!(!app.edit(Edit::Rating(2)));
    run_until(
        &ctx,
        &mut app,
        "the server's answer",
        |_| {},
        |app| !sending(app, Field::Rating),
    );
    // The listing holds the item the server returned.
    assert_eq!(listed(&app, "a.jpg").rating, Some(4));
    assert_eq!(app.detail().unwrap().error(), None);
    // The sidecar on disk has it as xmp:Rating.
    let xmp = std::fs::read_to_string(server.root.join("a.jpg.xmp")).unwrap();
    assert!(xmp.contains("Rating=\"4\""), "{xmp}");
    let sc = server
        .lib
        .read_sidecar(Path::new("a.jpg"))
        .unwrap()
        .unwrap();
    assert_eq!(sc.rating.unwrap().value, 4);

    // Clicking the current star again clears the rating.
    assert!(app.edit(Edit::Rating(rating_after_click(Some(4), 4))));
    run_until(
        &ctx,
        &mut app,
        "the cleared rating",
        |_| {},
        |app| !sending(app, Field::Rating),
    );
    assert_eq!(listed(&app, "a.jpg").rating.unwrap_or(0), 0);
    let sc = server
        .lib
        .read_sidecar(Path::new("a.jpg"))
        .unwrap()
        .unwrap();
    assert_eq!(sc.rating.unwrap().value, 0);

    // Tags and the description go the same way.
    assert!(app.edit(Edit::Subjects(vec!["cat".into()])));
    assert!(app.edit(Edit::Description(Some("on the sofa".into()))));
    run_until(
        &ctx,
        &mut app,
        "the tags and the description",
        |_| {},
        |app| !sending(app, Field::Subjects) && !sending(app, Field::Description),
    );
    let item = listed(&app, "a.jpg");
    assert_eq!(item.subjects, ["cat"]);
    assert_eq!(item.description.as_deref(), Some("on the sofa"));
}

#[test]
fn read_only_item_shows_reason_and_no_editors() {
    let server = TestServer::start(&["a.jpg", "b.jpg"], |root| {
        std::fs::write(root.join("a.jpg.xmp"), "<not xml").unwrap();
    });
    let (_fx, ctx, mut app) = open_real(&server, 2);

    // Read-only in the listing: the reason shows, the editors do not, and nothing is sent.
    let a = listed(&app, "a.jpg").id.clone();
    app.select(&a);
    frame(&ctx, &mut app);
    let view = app.detail().unwrap();
    assert!(view.read_only_reason().is_some_and(|r| !r.is_empty()));
    assert!(!view.editors_shown());
    assert!(!app.edit(Edit::Rating(3)));
    assert!(!sending(&app, Field::Rating));
    assert_eq!(
        std::fs::read(server.root.join("a.jpg.xmp")).unwrap(),
        b"<not xml"
    );

    // Editable in the listing, but its sidecar broke since: the server answers READ_ONLY,
    // and the pane then shows the same view.
    let b = listed(&app, "b.jpg").id.clone();
    app.select(&b);
    frame(&ctx, &mut app);
    assert!(app.detail().unwrap().editors_shown());
    std::fs::write(server.root.join("b.jpg.xmp"), "<not xml").unwrap();
    assert!(app.edit(Edit::Rating(3)));
    run_until(
        &ctx,
        &mut app,
        "the server's refusal",
        |_| {},
        |app| !sending(app, Field::Rating),
    );
    let view = app.detail().unwrap();
    assert!(view.read_only_reason().is_some_and(|r| !r.is_empty()));
    assert!(!view.editors_shown());
    assert_eq!(view.error(), None);
    assert_eq!(
        std::fs::read(server.root.join("b.jpg.xmp")).unwrap(),
        b"<not xml"
    );
}

/// A stub listing one item, `x`, whose `media.update` answers come from `answers`, in turn
/// (the last one repeating). Returns the address and when each update arrived.
fn update_stub(answers: Vec<Value>) -> (std::net::SocketAddr, Arc<Mutex<Vec<Instant>>>) {
    let updates = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let log = updates.clone();
    let addr = serve(move |req| {
        let Some((id, method)) = req.rpc() else {
            return StubResponse::new(404, Vec::new());
        };
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" => StubResponse::rpc_result(id, library_info("lib")),
            "folder.list" => StubResponse::rpc_result(id, listing(&[], &["x"])),
            "media.update" => {
                let mut seen = log.lock().unwrap();
                seen.push(Instant::now());
                let answer = &answers[(seen.len() - 1).min(answers.len() - 1)];
                answer_with(id, answer.clone(), params(req))
            }
            _ => StubResponse::new(404, Vec::new()),
        }
    });
    (addr, updates)
}

/// `answer` is an error code, or `"ok"` to answer with the item updated by `params`.
fn answer_with(id: u64, answer: Value, params: Value) -> StubResponse {
    if let Some(code) = answer.as_i64() {
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": "refused"},
        });
        return StubResponse::new(200, body.to_string().into_bytes())
            .header("content-type", "application/json");
    }
    let mut item = item("x");
    if let Some(r) = params.get("rating") {
        item["rating"] = r.clone();
    }
    StubResponse::rpc_result(id, item)
}

fn open_stub(addr: std::net::SocketAddr) -> (Fixture, egui::Context, PhotosApp) {
    let (fx, _) = fixture(&[("stub", addr)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "the listing",
        |_| {},
        |app| item_ids(app) == ["x"],
    );
    app.select(&ItemId("x".into()));
    (fx, ctx, app)
}

#[test]
fn busy_is_retried_once() {
    // Busy, then done: sent again once, after a short wait, and the editor waits with it.
    let (addr, updates) = update_stub(vec![json!(codes::BUSY), json!("ok")]);
    let (_fx, ctx, mut app) = open_stub(addr);
    assert!(app.edit(Edit::Rating(5)));
    run_until(
        &ctx,
        &mut app,
        "the retried update",
        |app| {
            // Still sending until the retry's answer is in.
            let done = listed(app, "x.jpg").rating == Some(5);
            assert_eq!(sending(app, Field::Rating), !done);
        },
        |app| listed(app, "x.jpg").rating == Some(5),
    );
    let times = updates.lock().unwrap().clone();
    assert_eq!(times.len(), 2);
    assert!(times[1] - times[0] >= BUSY_RETRY_DELAY - Duration::from_millis(50));
    assert_eq!(app.detail().unwrap().error(), None);

    // Busy twice: no third try; the error shows and the editor is usable again.
    let (addr, updates) = update_stub(vec![json!(codes::BUSY)]);
    let (_fx, ctx, mut app) = open_stub(addr);
    assert!(app.edit(Edit::Rating(5)));
    run_until(
        &ctx,
        &mut app,
        "the second refusal",
        |_| {},
        |app| !sending(app, Field::Rating),
    );
    assert_eq!(app.detail().unwrap().error(), Some(&EditError::Busy));
    assert_eq!(listed(&app, "x.jpg").rating, None);
    // Watch a while longer than the retry delay: nothing more is sent.
    let watch = Instant::now();
    while watch.elapsed() < BUSY_RETRY_DELAY * 2 {
        frame(&ctx, &mut app);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(updates.lock().unwrap().len(), 2);
    // A new edit clears the error and is sent.
    assert!(app.edit(Edit::Rating(1)));
    assert_eq!(app.detail().unwrap().error(), None);
}

#[test]
fn other_errors_reenable_the_editors_and_keep_the_server_value() {
    let (addr, updates) = update_stub(vec![json!(codes::INVALID_PARAMS)]);
    let (_fx, ctx, mut app) = open_stub(addr);
    assert!(app.edit(Edit::Rating(5)));
    run_until(
        &ctx,
        &mut app,
        "the refusal",
        |_| {},
        |app| !sending(app, Field::Rating),
    );
    let view = app.detail().unwrap();
    assert!(matches!(view.error(), Some(EditError::Failed(_))));
    assert!(view.editors_shown());
    assert_eq!(listed(&app, "x.jpg").rating, None);
    assert_eq!(updates.lock().unwrap().len(), 1);
}

#[test]
fn an_edit_answered_after_a_switch_is_dropped() {
    // A holds its media.update until the test lets it go.
    let (asked_tx, asked) = mpsc::channel::<()>();
    let (release_tx, release) = mpsc::channel::<()>();
    let release = Mutex::new(release);
    let (written_tx, written) = mpsc::channel::<()>();
    let written_tx = Mutex::new(written_tx);
    let a = serve(move |req| {
        let Some((id, method)) = req.rpc() else {
            return StubResponse::new(404, Vec::new());
        };
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" => StubResponse::rpc_result(id, library_info("lib-a")),
            "folder.list" => StubResponse::rpc_result(id, listing(&[], &["x"])),
            "media.update" => {
                asked_tx.send(()).unwrap();
                release.lock().unwrap().recv().unwrap();
                let tx = written_tx.lock().unwrap().clone();
                let mut resp = answer_with(id, json!("ok"), params(req));
                resp.after_write = Some(Box::new(move || {
                    let _ = tx.send(());
                }));
                resp
            }
            _ => StubResponse::new(404, Vec::new()),
        }
    });
    // B lists an item with the same id.
    let (b, _) = stub("lib-b", |_, id| {
        StubResponse::rpc_result(id, listing(&[], &["x"]))
    });
    let (fx, ids) = fixture(&[("A", a), ("B", b)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "A's listing",
        |_| {},
        |app| item_ids(app) == ["x"],
    );
    app.select(&ItemId("x".into()));
    assert!(app.edit(Edit::Rating(5)));
    run_until(
        &ctx,
        &mut app,
        "A's update",
        |_| {},
        |_| asked.try_recv().is_ok(),
    );
    app.switch_to(ids[1]);
    run_until(
        &ctx,
        &mut app,
        "B's listing",
        |_| {},
        |app| item_ids(app) == ["x"],
    );
    release_tx.send(()).unwrap();
    written.recv_timeout(support::common::DEADLINE).unwrap();
    let watch = Instant::now();
    while watch.elapsed() < Duration::from_millis(300) {
        frame(&ctx, &mut app);
        assert_eq!(listed(&app, "x.jpg").rating, None);
        std::thread::sleep(Duration::from_millis(5));
    }
    // Nothing of A's edit is waited for on B.
    app.select(&ItemId("x".into()));
    assert!(!sending(&app, Field::Rating));
}

#[test]
fn the_viewer_steps_through_the_listing_and_closes_on_escape() {
    let (addr, _) = stub("lib", |_, id| {
        StubResponse::rpc_result(id, listing(&[], &["a", "b", "c"]))
    });
    let (fx, _) = fixture(&[("A", addr)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "the listing",
        |_| {},
        |app| item_ids(app).len() == 3,
    );
    let id = |s: &str| ItemId(s.into());
    let selected = |app: &PhotosApp| app.connection().unwrap().browse().selected().cloned();
    app.select(&id("a"));
    frame(&ctx, &mut app);
    // Enter on the selection opens it full-screen.
    frame_with(&ctx, &mut app, vec![key(egui::Key::Enter)]);
    assert_eq!(app.opened_item(), Some(&id("a")));
    frame_with(&ctx, &mut app, vec![key(egui::Key::ArrowRight)]);
    assert_eq!(app.opened_item(), Some(&id("b")));
    assert_eq!(selected(&app), Some(id("b")));
    frame_with(&ctx, &mut app, vec![key(egui::Key::ArrowRight)]);
    frame_with(&ctx, &mut app, vec![key(egui::Key::ArrowRight)]);
    // The last item stays at the end.
    assert_eq!(app.opened_item(), Some(&id("c")));
    frame_with(&ctx, &mut app, vec![key(egui::Key::ArrowLeft)]);
    assert_eq!(app.opened_item(), Some(&id("b")));
    frame_with(&ctx, &mut app, vec![key(egui::Key::Escape)]);
    assert_eq!(app.opened_item(), None);
    // The selection follows the viewer.
    assert_eq!(selected(&app), Some(id("b")));
}

#[test]
fn rating_clicks_set_or_clear() {
    assert_eq!(rating_after_click(None, 3), 3);
    assert_eq!(rating_after_click(Some(0), 1), 1);
    assert_eq!(rating_after_click(Some(2), 4), 4);
    assert_eq!(rating_after_click(Some(4), 4), 0);
}
