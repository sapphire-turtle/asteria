//! What the app's integration tests share: a stub music server (built on the client crate's
//! HTTP stub, shared through `#[path]`), a servers.toml to start from, and driving frames
//! headlessly.

// Each test binary uses its own subset of these helpers.
#![allow(dead_code)]

#[path = "../../../../../crates/asteria-common-client/tests/common/mod.rs"]
pub mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asteria_common_client::profiles::{ProfileId, Profiles, Token};
use asteria_music_api::{ItemId, MediaItem, MediaKind};
use asteria_music_gui::app::{AppPaths, MusicApp};
use asteria_music_gui::player::{HttpOpener, OpenerSettings, Player, QueueEntry};
use corundum_framework::egui;
use corundum_framework_audio::fakes::{FakeItem, FakeOpener, FakeOutputs};
use corundum_framework_audio::{Feed, Opener, OutputConfig};
use serde_json::{Value, json};

pub use common::offline::file_hash;
use common::{DEADLINE, StubResponse, library_info, serve};

/// The `server.info` result of an asteria-music server.
pub fn music_server_info() -> Value {
    json!({
        "app": "asteria-music",
        "app_api": 2,
        "server_version": "0.0.0",
        "epoch": "e1",
    })
}

pub fn media(id: &str) -> Value {
    json!({
        "id": id,
        "path": format!("{id}.flac"),
        "kind": "audio",
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

pub fn track(id: &str, number: u32, artist: &str) -> Value {
    json!({
        "item": media(id),
        "tags": {
            "title": id,
            "artists": [artist],
            "album": "Album",
            "album_artist": "Band",
            "track": number,
            "disc": 1,
            "duration_ms": 61_000,
        },
        "display_title": id,
    })
}

pub fn album_summary(id: &str) -> Value {
    json!({
        "id": id,
        "title": format!("Title {id}"),
        "album_artist": "Band",
        "album_artist_id": "band",
        "year": 2020,
        "track_count": 4,
        "cover_item": null,
    })
}

pub fn entry(path: &str, status: &str, track: Option<Value>) -> Value {
    json!({"raw_path": path, "title_hint": null, "status": status, "track": track})
}

/// What the stub serves. Every RPC method and its params are recorded in `seen`.
#[derive(Clone)]
pub struct Music {
    /// How many albums `album.list` has (in pages of the limit asked for).
    pub albums: usize,
    /// Prefix of the album ids, to tell two servers apart.
    pub prefix: &'static str,
    /// How long `album.list` takes to answer.
    pub album_delay: Duration,
    /// `album.list` answers with something that is not an album listing.
    pub bad_albums: bool,
    /// The next this many `album.list` calls for a page after the first fail.
    pub fail_later_pages: Arc<std::sync::atomic::AtomicU32>,
    /// An album's tracks carry file hashes ([`file_hash`] of their ids), and their files are
    /// served for offline (`/media/<id>/<preset>`), but for the second track of each album
    /// (`<album>-t1`), which is 404.
    pub offline_media: bool,
}

impl Default for Music {
    fn default() -> Self {
        Music {
            albums: 3,
            prefix: "al",
            album_delay: Duration::ZERO,
            bad_albums: false,
            fail_later_pages: Arc::default(),
            offline_media: false,
        }
    }
}

pub type Seen = Arc<Mutex<Vec<(String, Value)>>>;

pub fn calls(seen: &Seen, method: &str) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == method)
        .map(|(_, p)| p.clone())
        .collect()
}

impl Music {
    pub fn serve(self) -> (SocketAddr, Seen) {
        let seen: Seen = Arc::default();
        let log = seen.clone();
        let addr = serve(move |req| {
            let Some((id, method)) = req.rpc() else {
                // A file's fetch: recorded by its path, and refused unless saved for offline.
                log.lock()
                    .unwrap()
                    .push((req.method.clone(), json!(req.path)));
                let served = self.offline_media
                    && req
                        .path
                        .strip_prefix("/media/")
                        .and_then(|rest| rest.split_once('/'))
                        .is_some_and(|(item, _)| !item.ends_with("-t1"));
                return if served {
                    StubResponse::new(200, b"saved".to_vec())
                } else {
                    StubResponse::new(404, Vec::new())
                };
            };
            let params = serde_json::from_slice::<Value>(&req.body).unwrap()["params"].clone();
            log.lock().unwrap().push((method.clone(), params.clone()));
            let result = match method.as_str() {
                "server.info" => music_server_info(),
                "library.info" => library_info("lib"),
                "album.list" => {
                    std::thread::sleep(self.album_delay);
                    use std::sync::atomic::Ordering::SeqCst;
                    if !params["cursor"].is_null()
                        && self
                            .fail_later_pages
                            .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                            .is_ok()
                    {
                        return StubResponse::new(500, Vec::new());
                    }
                    if self.bad_albums {
                        json!({"albums": 5})
                    } else {
                        self.album_page(&params)
                    }
                }
                "album.get" => {
                    let aid = params["id"].as_str().unwrap().to_string();
                    json!({
                        "summary": album_summary(&aid),
                        "folder": "Band/Album",
                        "tracks": (0..4).map(|n| {
                            let artist = if n == 1 { "Guest" } else { "Band" };
                            let tid = format!("{aid}-t{n}");
                            let mut t = track(&tid, n + 1, artist);
                            if self.offline_media {
                                t["item"]["file_hash"] = json!(file_hash(&tid));
                            }
                            t
                        }).collect::<Vec<_>>(),
                    })
                }
                "artist.list" => json!({
                    "artists": [
                        {"id": "band", "name": "Band", "album_count": 2},
                        {"id": "solo", "name": "Solo", "album_count": 1},
                    ],
                    "next_cursor": null,
                }),
                "artist.get" => json!({
                    "summary": {"id": "band", "name": "Band", "album_count": 2},
                    "albums": [album_summary("own1"), album_summary("own2")],
                    "appears_on": [album_summary("guest1")],
                }),
                "playlist.list" => json!({
                    "playlists": [{"id": "pl", "name": "Mix", "path": "mix.m3u"}],
                    "next_cursor": null,
                }),
                "playlist.get" => json!({
                    "summary": {"id": "pl", "name": "Mix", "path": "mix.m3u"},
                    "entries": [
                        entry("a.flac", "found", Some(track("pa", 1, "Band"))),
                        entry("gone.flac", "missing", None),
                        entry("moved.flac", "moved", Some(track("pm", 2, "Band"))),
                        entry("d.flac", "found", Some(track("pd", 3, "Band"))),
                    ],
                    "truncated": false,
                }),
                _ => return StubResponse::new(404, Vec::new()),
            };
            StubResponse::rpc_result(id, result)
        });
        (addr, seen)
    }

    fn album_page(&self, params: &Value) -> Value {
        let start = params["cursor"]
            .as_str()
            .map(|c| c.parse::<usize>().unwrap())
            .unwrap_or(0);
        let limit = params["limit"].as_u64().unwrap_or(200) as usize;
        let end = (start + limit).min(self.albums);
        json!({
            "albums": (start..end).map(|n| album_summary(&format!("{}{n}", self.prefix))).collect::<Vec<_>>(),
            "next_cursor": (end < self.albums).then(|| end.to_string()),
        })
    }
}

pub struct Fixture {
    _dir: tempfile::TempDir,
    pub paths: AppPaths,
}

/// A servers.toml with `servers` (name, address), the one at `active` in use.
pub fn fixture(servers: &[(&str, SocketAddr)], active: usize) -> (Fixture, Vec<ProfileId>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config/servers.toml");
    let mut profiles = Profiles::default();
    let ids: Vec<ProfileId> = servers
        .iter()
        .map(|(name, addr)| {
            profiles
                .add(*name, &format!("http://{addr}"), Token::new("t"))
                .unwrap()
        })
        .collect();
    profiles.set_active(Some(ids[active])).unwrap();
    profiles.save(&path).unwrap();
    let paths = AppPaths {
        servers: path,
        playback: dir.path().join("config/playback.toml"),
        cache: Some(dir.path().join("cache")),
        data: Some(dir.path().join("data")),
    };
    (Fixture { _dir: dir, paths }, ids)
}

/// A queue entry for `id`, titled `id`, with the file `<id>.flac`.
pub fn queue_entry(id: &str) -> QueueEntry {
    QueueEntry {
        media: MediaItem {
            id: ItemId(id.into()),
            path: format!("{id}.flac"),
            kind: MediaKind::Audio,
            size: 0,
            modified: "2026-01-01T00:00:00Z".into(),
            rating: None,
            subjects: vec![],
            hierarchical_subjects: vec![],
            description: None,
            label: None,
            content_hash: None,
            file_hash: Some(format!("blake3:{id}")),
            read_only_reason: None,
        },
        title: id.into(),
        artist: String::new(),
        album: String::new(),
        duration_ms: None,
    }
}

/// corundum's fake opener for queue entries: an entry is the fake item of its media id, so
/// the tests script tracks by id.
pub struct EntryOpener(pub FakeOpener);

fn fake_item(e: &QueueEntry) -> FakeItem {
    FakeItem {
        id: e.media.id.0.clone(),
        title: e.title.clone(),
        duration_ms: e.duration_ms,
    }
}

impl Opener<QueueEntry> for EntryOpener {
    fn open(&mut self, entry: &QueueEntry, at_ms: u64, target: OutputConfig) -> Box<dyn Feed> {
        self.0.open(&fake_item(entry), at_ms, target)
    }

    fn preload(&mut self, next: Option<&QueueEntry>) {
        self.0.preload(next.map(fake_item).as_ref());
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// A player on the fakes of the device and the opener, and the settings the app is handed.
pub fn player(ctx: &egui::Context) -> (Player, OpenerSettings) {
    let (player, settings, _, _) = player_with_fakes(ctx);
    (player, settings)
}

/// Like [`player`], with the fakes the test can look at.
pub fn player_with_fakes(ctx: &egui::Context) -> (Player, OpenerSettings, FakeOpener, FakeOutputs) {
    let opener = FakeOpener::new();
    let outputs = FakeOutputs::new();
    let ctx = ctx.clone();
    let player = Player::spawn(
        Box::new(EntryOpener(opener.clone())),
        Box::new(outputs.clone()),
        move || ctx.request_repaint(),
    )
    .unwrap();
    (player, OpenerSettings::new(), opener, outputs)
}

/// A player that fetches from the server through the real opener, on the fake device.
pub fn player_with_http(ctx: &egui::Context) -> (Player, OpenerSettings) {
    let settings = OpenerSettings::new();
    let ctx = ctx.clone();
    let player = Player::spawn(
        Box::new(HttpOpener::new(settings.clone())),
        Box::new(FakeOutputs::new()),
        move || ctx.request_repaint(),
    )
    .unwrap();
    (player, settings)
}

/// Waits (a few seconds) for `ok` to hold.
pub fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn start(addr: SocketAddr) -> (Fixture, egui::Context, MusicApp) {
    let (fx, _) = fixture(&[("A", addr)], 0);
    let ctx = egui::Context::default();
    let (player, settings) = player(&ctx);
    let app = MusicApp::new(fx.paths.clone(), &ctx, player, settings);
    (fx, ctx, app)
}

pub fn frame(ctx: &egui::Context, app: &mut MusicApp) {
    frame_sized(ctx, app, egui::vec2(1000.0, 700.0));
}

pub fn frame_sized(ctx: &egui::Context, app: &mut MusicApp, size: egui::Vec2) {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        },
        |ui| app.show(ui),
    );
    // There is no renderer to hand the texture uploads to.
    output.textures_delta.clear();
}

/// Runs frames until `done` holds.
pub fn run_until(
    ctx: &egui::Context,
    app: &mut MusicApp,
    what: &str,
    mut done: impl FnMut(&MusicApp) -> bool,
) {
    let start = Instant::now();
    loop {
        frame(ctx, app);
        if done(app) {
            return;
        }
        assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
