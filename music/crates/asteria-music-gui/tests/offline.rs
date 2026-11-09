//! The app without its server: an album saved for offline still opens, its saved tracks play
//! from the copies, and the ones not saved are left out of the queue.

mod support;

use asteria_common_client::offline::{PinStatus, PinTarget, SyncPhase};
use asteria_common_client::profiles::Profiles;
use asteria_common_client::session::SessionState;
use asteria_music_api::AlbumId;
use asteria_music_gui::app::MusicApp;
use asteria_music_gui::library::TrackList;
use asteria_music_gui::offline::PIN_ALBUM;
use asteria_music_gui::player::OpenerSettings;
use corundum_framework::egui;

use support::common::free_addr;
use support::*;

fn open_app(fx: &Fixture) -> (egui::Context, MusicApp, OpenerSettings) {
    let ctx = egui::Context::default();
    let (player, settings, _opener, _outs) = player_with_fakes(&ctx);
    let app = MusicApp::new(fx.paths.clone(), &ctx, player, settings.clone());
    (ctx, app, settings)
}

/// Connects to a stub that serves the files for offline, saves the album `al1` and waits for
/// the sync to finish; then moves the server's address to one nothing listens on, as if the
/// server had stopped.
fn save_album_then_lose_the_server() -> Fixture {
    let (addr, _) = Music {
        offline_media: true,
        ..Default::default()
    }
    .serve();
    let (fx, ids) = fixture(&[("A", addr)], 0);
    let (ctx, mut app, _) = open_app(&fx);
    run_until(&ctx, &mut app, "the albums", |a| {
        !a.connection().unwrap().library().albums().is_empty()
    });
    let handle = app
        .connection()
        .unwrap()
        .session()
        .offline()
        .expect("an offline handle")
        .clone();
    handle.pin(PinTarget {
        kind: PIN_ALBUM.into(),
        id: "al1".into(),
        name: "Title al1".into(),
    });
    run_until(&ctx, &mut app, "the album to be saved", |_| {
        handle.status(PIN_ALBUM, "al1") == Some(PinStatus::Saved)
            && handle.progress().phase == SyncPhase::Idle
            && handle.is_stored(&file_hash("al1-t3"), "offline-audio")
    });
    drop(app);

    let mut profiles = Profiles::load(&fx.paths.servers).unwrap();
    let p = profiles.active().unwrap().clone();
    let gone = format!("http://{}", free_addr());
    profiles.update(ids[0], p.name, &gone, p.token).unwrap();
    profiles.save(&fx.paths.servers).unwrap();
    fx
}

fn offline(ctx: &egui::Context, app: &mut MusicApp) {
    run_until(ctx, app, "the session to go offline", |a| {
        matches!(
            a.connection().unwrap().session().state(),
            SessionState::Offline { .. }
        )
    });
}

#[test]
fn a_saved_album_opens_with_the_server_gone() {
    let fx = save_album_then_lose_the_server();
    let (ctx, mut app, settings) = open_app(&fx);
    offline(&ctx, &mut app);
    run_until(&ctx, &mut app, "the saved album in the list", |a| {
        a.connection()
            .unwrap()
            .library()
            .albums()
            .iter()
            .any(|al| al.id.0 == "al1")
    });
    app.open_album(AlbumId("al1".into()));
    run_until(&ctx, &mut app, "the album", |a| {
        a.connection().unwrap().library().album().is_some()
    });
    let lib = app.connection().unwrap().library();
    assert_eq!(lib.album().unwrap().tracks.len(), 4);
    // The player is told where the copies are, and that the server is gone.
    wait_until("the endpoint", || settings.endpoint().is_some());
    assert!(settings.endpoint().unwrap().offline.is_some());
    assert!(!settings.online());
}

#[test]
fn unsaved_tracks_are_skipped_when_queued_offline() {
    let fx = save_album_then_lose_the_server();
    let (ctx, mut app, settings) = open_app(&fx);
    offline(&ctx, &mut app);
    // Handing the player its endpoint resets it: wait for that before queueing.
    run_until(&ctx, &mut app, "the endpoint", |_| {
        settings.endpoint().is_some()
    });
    app.open_album(AlbumId("al1".into()));
    run_until(&ctx, &mut app, "the album", |a| {
        a.connection().unwrap().library().album().is_some()
    });
    // The second track's file was never served, so it is not saved.
    app.play_from(TrackList::Album, 1);
    app.play_from(TrackList::Album, 2);
    wait_until("the queue", || !app.player().state().queue.is_empty());
    let state = app.player().state();
    let titles: Vec<&str> = state.queue.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, ["al1-t0", "al1-t2", "al1-t3"]);
}
