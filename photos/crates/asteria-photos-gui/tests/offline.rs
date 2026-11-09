//! The app without its server, and "Show in low quality": against a real photos server on a
//! temporary library. A folder saved for offline still opens with its thumbnails once the
//! server is gone, and the viewer draws `offline-image` offline or when the setting is on.

mod support;

use asteria_common_api::{ItemId, MediaItem};
use asteria_common_client::offline::{PinStatus, PinTarget, SyncPhase};
use asteria_common_client::profiles::Profiles;
use asteria_common_client::session::SessionState;
use asteria_photos_gui::PhotosApp;
use asteria_photos_gui::offline::PIN_FOLDER;
use corundum_framework::egui;

use support::common::free_addr;
use support::server::{TestServer, red_jpg};
use support::*;

/// A library with `top.jpg` at the root and `Inbox/a.jpg`, plus `Inbox/broken.jpg`, which
/// the server cannot render (so it is never saved).
fn library() -> TestServer {
    TestServer::start(&["top.jpg"], |root| {
        std::fs::create_dir(root.join("Inbox")).unwrap();
        std::fs::copy(red_jpg(), root.join("Inbox/a.jpg")).unwrap();
        std::fs::write(root.join("Inbox/broken.jpg"), b"not a picture").unwrap();
    })
}

fn listed(app: &PhotosApp, path: &str) -> Option<MediaItem> {
    let items = app.connection()?.browse().items();
    items.iter().find(|i| i.path == path).cloned()
}

/// Opens `folder` and waits for the item at `path` in it.
fn open_and_find(ctx: &egui::Context, app: &mut PhotosApp, folder: &str, path: &str) -> MediaItem {
    app.open_folder(folder);
    run_until(ctx, app, path, |_| {}, |a| listed(a, path).is_some());
    listed(app, path).unwrap()
}

/// Saves `Inbox` for offline on `server` and waits for its copies; then stops the server and
/// moves its address to one nothing listens on. Returns the saved item `Inbox/a.jpg`.
fn save_inbox_then_lose_the_server(server: TestServer) -> (Fixture, MediaItem) {
    let (fx, ids) = fixture_with_tokens(&[("real", server.addr, &server.token)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    let item = open_and_find(&ctx, &mut app, "Inbox", "Inbox/a.jpg");
    let hash = item.file_hash.clone().expect("the server hashed the file");
    let handle = app
        .connection()
        .unwrap()
        .session()
        .offline()
        .expect("an offline handle")
        .clone();
    handle.pin(PinTarget {
        kind: PIN_FOLDER.into(),
        id: "Inbox".into(),
        name: "Inbox".into(),
    });
    // Waited for by hand, so a timeout says where the sync stood.
    let start = std::time::Instant::now();
    loop {
        frame(&ctx, &mut app);
        let status = handle.status(PIN_FOLDER, "Inbox");
        let phase = handle.progress().phase;
        let stored = (
            handle.is_stored(&hash, "thumb"),
            handle.is_stored(&hash, "offline-image"),
        );
        if status == Some(PinStatus::Saved) && phase == SyncPhase::Idle && stored == (true, true) {
            break;
        }
        assert!(
            start.elapsed() < support::common::DEADLINE,
            "Inbox was never saved: {status:?}, {phase:?}, stored (thumb, image) {stored:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    drop(app);
    drop(server);

    let mut profiles = Profiles::load(&fx.paths.servers).unwrap();
    let p = profiles.active().unwrap().clone();
    let gone = format!("http://{}", free_addr());
    profiles.update(ids[0], p.name, &gone, p.token).unwrap();
    profiles.save(&fx.paths.servers).unwrap();
    (fx, item)
}

/// The app on `fx`, once its session has gone offline.
fn open_offline(fx: &Fixture) -> (egui::Context, PhotosApp) {
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "the session to go offline",
        |_| {},
        |a| {
            matches!(
                a.connection().unwrap().session().state(),
                SessionState::Offline { .. }
            )
        },
    );
    (ctx, app)
}

/// Selects `id` and opens it full screen with Enter.
fn open_in_viewer(ctx: &egui::Context, app: &mut PhotosApp, id: &ItemId) {
    app.select(id);
    frame(ctx, app);
    frame_with(ctx, app, vec![key(egui::Key::Enter)]);
}

fn texture(item: &MediaItem, preset: &str) -> String {
    format!("asteria-media/{}/{preset}", item.id.0)
}

#[test]
fn a_saved_folder_opens_with_the_server_gone() {
    let (fx, item) = save_inbox_then_lose_the_server(library());
    let (ctx, mut app) = open_offline(&fx);
    // The root is not saved, but leads down to what is.
    run_until(
        &ctx,
        &mut app,
        "Inbox at the root",
        |_| {},
        |a| {
            let b = a.connection().unwrap().browse();
            b.path().is_empty() && b.folders() == ["Inbox"]
        },
    );
    let found = open_and_find(&ctx, &mut app, "Inbox", "Inbox/a.jpg");
    assert_eq!(found.id, item.id);
    run_until(
        &ctx,
        &mut app,
        "the saved thumbnail",
        |_| {},
        |_| has_texture(&ctx, &texture(&item, "thumb")),
    );
}

#[test]
fn low_quality_shows_offline_image() {
    let server = library();
    let (fx, _) = fixture_with_tokens(&[("real", server.addr, &server.token)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    let item = open_and_find(&ctx, &mut app, "", "top.jpg");
    app.set_low_quality(true);
    assert!(app.display_settings().low_quality);
    open_in_viewer(&ctx, &mut app, &item.id);
    assert_eq!(app.opened_item(), Some(&item.id));
    run_until(
        &ctx,
        &mut app,
        "the offline-image preview",
        |_| {},
        |_| has_texture(&ctx, &texture(&item, "offline-image")),
    );
    let asked = format!("/media/{}/offline-image", item.id.0);
    assert!(server.was_asked(&asked), "the viewer asked for {asked}");
    let preview = format!("/media/{}/preview", item.id.0);
    assert!(!server.was_asked(&preview), "nothing asked for {preview}");
    // The setting is kept in display.toml.
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(&fx.paths.display).is_ok_and(|t| t.contains("true")) {
        assert!(start.elapsed() < support::common::DEADLINE, "display.toml");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn offline_shows_offline_image_even_with_the_setting_off() {
    let (fx, item) = save_inbox_then_lose_the_server(library());
    let (ctx, mut app) = open_offline(&fx);
    assert!(!app.display_settings().low_quality);
    open_and_find(&ctx, &mut app, "Inbox", "Inbox/a.jpg");
    open_in_viewer(&ctx, &mut app, &item.id);
    assert_eq!(app.opened_item(), Some(&item.id));
    run_until(
        &ctx,
        &mut app,
        "the saved offline-image",
        |_| {},
        |_| has_texture(&ctx, &texture(&item, "offline-image")),
    );
    assert!(!has_texture(&ctx, &texture(&item, "preview")));
}

#[test]
fn an_item_not_saved_does_not_open_offline() {
    let (fx, _) = save_inbox_then_lose_the_server(library());
    let (ctx, mut app) = open_offline(&fx);
    let broken = open_and_find(&ctx, &mut app, "Inbox", "Inbox/broken.jpg");
    open_in_viewer(&ctx, &mut app, &broken.id);
    frame(&ctx, &mut app);
    assert_eq!(app.opened_item(), None);
}
