//! What the app's integration tests share: stub servers (the client crate's test stub,
//! shared through `#[path]`), a servers.toml to start from, and driving frames headlessly.

// Each test binary uses its own subset of these helpers.
#![allow(dead_code)]

#[path = "../../../crates/asteria-framework-client/tests/common/mod.rs"]
pub mod common;
pub mod server;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asteria_framework_client::profiles::{ProfileId, Profiles, Token};
use asteria_photos::app::{AppPaths, PhotosApp};
use corundum_framework::egui;
use serde_json::{Value, json};

use common::{DEADLINE, StubRequest, StubResponse, library_info, photos_server_info, serve};

pub fn item(id: &str) -> Value {
    json!({
        "id": id,
        "path": format!("{id}.jpg"),
        "kind": "image",
        "size": 1,
        "modified": "2026-01-01T00:00:00Z",
        "rating": null,
        "subjects": [],
        "hierarchical_subjects": [],
        "description": null,
        "label": null,
        "content_hash": null,
        "file_hash": null,
        "read_only_reason": null,
    })
}

pub fn listing(folders: &[&str], items: &[&str]) -> Value {
    json!({
        "folders": folders,
        "items": items.iter().map(|i| item(i)).collect::<Vec<_>>(),
        "next_cursor": null,
    })
}

pub fn params(req: &StubRequest) -> Value {
    serde_json::from_slice::<Value>(&req.body).unwrap()["params"].clone()
}

/// Answers the handshake for `library`, `folder.list` with `list`, and anything else
/// (thumbnails) with 404. Every RPC method it receives is recorded in `seen`.
pub fn stub(
    library: &'static str,
    list: impl Fn(&StubRequest, u64) -> StubResponse + Send + Sync + 'static,
) -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let addr = serve(move |req| {
        let Some((id, method)) = req.rpc() else {
            return StubResponse::new(404, Vec::new());
        };
        log.lock().unwrap().push(method.clone());
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" => StubResponse::rpc_result(id, library_info(library)),
            "folder.list" => list(req, id),
            _ => StubResponse::new(404, Vec::new()),
        }
    });
    (addr, seen)
}

pub struct Fixture {
    _dir: tempfile::TempDir,
    pub paths: AppPaths,
}

/// A servers.toml with `servers` (name, address), the one at `active` in use.
pub fn fixture(
    servers: &[(&str, std::net::SocketAddr)],
    active: usize,
) -> (Fixture, Vec<ProfileId>) {
    let with_token: Vec<_> = servers.iter().map(|(n, a)| (*n, *a, "t")).collect();
    fixture_with_tokens(&with_token, active)
}

/// As [`fixture`], each server with its own token.
pub fn fixture_with_tokens(
    servers: &[(&str, std::net::SocketAddr, &str)],
    active: usize,
) -> (Fixture, Vec<ProfileId>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config/servers.toml");
    let mut profiles = Profiles::default();
    let ids: Vec<ProfileId> = servers
        .iter()
        .map(|(name, addr, token)| {
            profiles
                .add(*name, &format!("http://{addr}"), Token::new(*token))
                .unwrap()
        })
        .collect();
    profiles.set_active(Some(ids[active])).unwrap();
    profiles.save(&path).unwrap();
    let paths = AppPaths {
        servers: path,
        cache: Some(dir.path().join("cache")),
    };
    (Fixture { _dir: dir, paths }, ids)
}

pub fn frame(ctx: &egui::Context, app: &mut PhotosApp) {
    frame_with(ctx, app, Vec::new());
}

/// One frame in which `events` (key presses, say) arrive.
pub fn frame_with(ctx: &egui::Context, app: &mut PhotosApp, events: Vec<egui::Event>) {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            events,
            ..Default::default()
        },
        |ui| app.show(ui),
    );
    // There is no renderer to hand the texture uploads to.
    output.textures_delta.clear();
}

/// Runs frames until `done` holds, checking `always` after every frame.
pub fn run_until(
    ctx: &egui::Context,
    app: &mut PhotosApp,
    what: &str,
    mut always: impl FnMut(&PhotosApp),
    mut done: impl FnMut(&PhotosApp) -> bool,
) {
    let start = Instant::now();
    loop {
        frame(ctx, app);
        always(app);
        if done(app) {
            return;
        }
        assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn item_ids(app: &PhotosApp) -> Vec<String> {
    app.connection()
        .map(|c| c.browse().items().iter().map(|i| i.id.0.clone()).collect())
        .unwrap_or_default()
}

pub fn connected_to(app: &PhotosApp) -> Option<ProfileId> {
    app.connection().map(|c| c.session().profile())
}

pub fn saved_active(path: &Path) -> Option<ProfileId> {
    Profiles::load(path).unwrap().active().map(|p| p.id)
}

pub fn wait_saved(path: &Path, want: ProfileId) {
    let start = Instant::now();
    while saved_active(path) != Some(want) {
        assert!(
            start.elapsed() < DEADLINE,
            "servers.toml never named the switch"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A press of `key`, with no modifier, as egui receives it.
pub fn key(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}
