//! Playback in the app: the player is handed its endpoint, the bar and the queue send their
//! commands, the settings are kept, and the stream directory goes with the app. The device
//! and the opener are fakes (but where the real opener's fetch is what is tested); the server
//! is a stub.

mod support;

use asteria_music_gui::app::{MusicApp, Screen};
use asteria_music_gui::player::{Command, OpenerSettings, Quality, Status};
use asteria_music_gui::settings::PlaybackSettings;
use corundum_framework::egui;
use serde_json::json;

use support::*;

/// An app on a stub server with fakes for the player, and the settings its opener reads.
fn start_fakes(
    paths: &asteria_music_gui::app::AppPaths,
) -> (egui::Context, MusicApp, OpenerSettings) {
    let ctx = egui::Context::default();
    let (player, settings, _opener, _outs) = player_with_fakes(&ctx);
    let app = MusicApp::new(paths.clone(), &ctx, player, settings.clone());
    (ctx, app, settings)
}

fn ready(ctx: &egui::Context, app: &mut MusicApp, settings: &OpenerSettings) {
    run_until(ctx, app, "the first page", |a| {
        !a.connection().unwrap().library().albums().is_empty()
    });
    wait_until("the endpoint", || settings.endpoint().is_some());
}

fn queue_titles(app: &MusicApp) -> Vec<String> {
    app.player()
        .state()
        .queue
        .iter()
        .map(|e| e.title.clone())
        .collect()
}

fn play(app: &MusicApp, ids: &[&str]) {
    app.player().send(Command::Replace {
        items: ids.iter().map(|i| queue_entry(i)).collect(),
        start: 0,
    });
    wait_until("the queue", || {
        app.player().state().queue.len() == ids.len()
    });
}

#[test]
fn ready_session_hands_the_player_its_endpoint() {
    let (addr, _) = Music::default().serve();
    let (fx, _) = fixture(&[("A", addr)], 0);
    let (ctx, mut app, settings) = start_fakes(&fx.paths);
    assert!(
        settings.endpoint().is_none(),
        "not before the session is ready"
    );
    ready(&ctx, &mut app, &settings);
    // The stream directory is under the cache, in the library's own directory.
    let cache = fx.paths.cache.clone().unwrap();
    let streams: Vec<_> = std::fs::read_dir(cache.join("lib/stream"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(streams.len(), 1);
}

#[test]
fn switching_servers_stops_and_clears_the_player() {
    let (addr_a, _) = Music::default().serve();
    // Nothing listens here, so the second connection never gets ready.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let (fx, ids) = fixture(&[("A", addr_a), ("B", dead)], 0);
    let (ctx, mut app, settings) = start_fakes(&fx.paths);
    ready(&ctx, &mut app, &settings);
    play(&app, &["a", "b"]);
    wait_until("playing", || app.player().state().status == Status::Playing);
    app.switch_to(ids[1]);
    frame(&ctx, &mut app);
    wait_until("the player cleared", || {
        app.player().state().queue.is_empty() && settings.endpoint().is_none()
    });
    assert_eq!(app.player().state().status, Status::Stopped);
    assert_eq!(app.player().state().current, None);
    assert_eq!(app.player().state().error, None);
}

#[test]
fn the_bar_buttons_send_their_commands() {
    let (addr, _) = Music::default().serve();
    let (fx, _) = fixture(&[("A", addr)], 0);
    let (ctx, mut app, settings) = start_fakes(&fx.paths);
    ready(&ctx, &mut app, &settings);
    play(&app, &["a", "b", "c"]);
    wait_until("playing", || app.player().state().status == Status::Playing);
    app.toggle();
    wait_until("paused", || app.player().state().status == Status::Paused);
    app.toggle();
    wait_until("playing again", || {
        app.player().state().status == Status::Playing
    });
    app.next();
    wait_until("the next track", || app.player().state().current == Some(1));
    app.previous();
    wait_until("the previous track", || {
        app.player().state().current == Some(0)
    });
    app.seek(40);
    wait_until("the seek", || app.player().state().position_ms >= 40);
    app.set_volume(2.0);
    wait_until("the volume", || app.player().state().volume == 1.0);
    app.set_volume(0.25);
    wait_until("the volume", || app.player().state().volume == 0.25);
    // The bar and the queue draw with something queued, on every screen and when narrow.
    for screen in [Screen::Albums, Screen::Queue, Screen::Settings] {
        app.open_screen(screen);
        frame(&ctx, &mut app);
        frame_sized(&ctx, &mut app, egui::vec2(400.0, 700.0));
    }
}

#[test]
fn queue_rows_jump_move_and_remove() {
    let (addr, _) = Music::default().serve();
    let (fx, _) = fixture(&[("A", addr)], 0);
    let (ctx, mut app, settings) = start_fakes(&fx.paths);
    ready(&ctx, &mut app, &settings);
    play(&app, &["a", "b", "c"]);
    app.jump(2);
    wait_until("the jump", || app.player().state().current == Some(2));
    app.move_queue_entry(2, 0);
    wait_until("the move", || queue_titles(&app) == ["c", "a", "b"]);
    assert_eq!(app.player().state().current, Some(0));
    app.remove_queue_entry(1);
    wait_until("the removal", || queue_titles(&app) == ["c", "b"]);
}

#[test]
fn low_quality_and_volume_are_saved_and_restored() {
    let (addr, _) = Music::default().serve();
    let (fx, _) = fixture(&[("A", addr)], 0);
    {
        let (ctx, mut app, settings) = start_fakes(&fx.paths);
        ready(&ctx, &mut app, &settings);
        assert_eq!(settings.quality(), Quality::Original);
        app.set_low_quality(true);
        app.set_volume(0.4);
        assert_eq!(settings.quality(), Quality::Low);
        // Dropping the app waits for the last write.
    }
    assert_eq!(
        PlaybackSettings::load(&fx.paths.playback),
        PlaybackSettings {
            low_quality: true,
            volume: 0.4
        }
    );
    let (_ctx, app, settings) = start_fakes(&fx.paths);
    assert_eq!(settings.quality(), Quality::Low);
    wait_until("the volume sent", || app.player().state().volume == 0.4);
    assert!(app.playback_settings().low_quality);
}

#[test]
fn an_unreadable_playback_toml_gives_the_defaults() {
    let (addr, _) = Music::default().serve();
    let (fx, _) = fixture(&[("A", addr)], 0);
    std::fs::write(&fx.paths.playback, "volume = [not toml").unwrap();
    assert_eq!(
        PlaybackSettings::load(&fx.paths.playback),
        PlaybackSettings::default()
    );
    // A directory where the file should be cannot be read either.
    std::fs::remove_file(&fx.paths.playback).unwrap();
    std::fs::create_dir(&fx.paths.playback).unwrap();
    assert_eq!(
        PlaybackSettings::load(&fx.paths.playback),
        PlaybackSettings::default()
    );
    let (_ctx, app, settings) = start_fakes(&fx.paths);
    assert_eq!(app.playback_settings(), &PlaybackSettings::default());
    assert_eq!(settings.quality(), Quality::Original);
    wait_until("the volume sent", || app.player().state().volume == 1.0);
    // A missing file and an out-of-range volume.
    let missing = fx.paths.playback.with_file_name("none.toml");
    assert_eq!(
        PlaybackSettings::load(&missing),
        PlaybackSettings::default()
    );
    std::fs::write(&missing, "volume = -3.0\nlow_quality = true\n").unwrap();
    let loaded = PlaybackSettings::load(&missing);
    assert_eq!((loaded.volume, loaded.low_quality), (0.0, true));
}

#[test]
fn closing_the_app_removes_its_stream_directory() {
    let (addr, _) = Music::default().serve();
    let (fx, _) = fixture(&[("A", addr)], 0);
    let (ctx, mut app, settings) = start_fakes(&fx.paths);
    ready(&ctx, &mut app, &settings);
    let base = fx.paths.cache.clone().unwrap().join("lib/stream");
    let dirs: Vec<_> = std::fs::read_dir(&base).unwrap().flatten().collect();
    assert_eq!(dirs.len(), 1);
    let dir = dirs[0].path();
    // The marker file inside is why the directory is removed recursively.
    std::fs::write(dir.join("t.opus"), b"x").unwrap();
    drop(app);
    assert!(!dir.exists());
}

#[test]
fn low_quality_asks_for_offline_audio_at_the_next_fetch() {
    let (addr, seen) = Music::default().serve();
    let (fx, _) = fixture(&[("A", addr)], 0);
    let ctx = egui::Context::default();
    let (player, settings) = player_with_http(&ctx);
    let mut app = MusicApp::new(fx.paths.clone(), &ctx, player, settings.clone());
    ready(&ctx, &mut app, &settings);
    app.set_low_quality(true);
    play(&app, &["a"]);
    wait_until("the fetch", || !calls(&seen, "GET").is_empty());
    assert_eq!(calls(&seen, "GET")[0], json!("/media/a/offline-audio"));
}
