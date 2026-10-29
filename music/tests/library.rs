//! The browse screens against a stub music server: paging, opening, what playing builds,
//! and what a stale or broken reply does.

mod support;

use std::time::Duration;

use asteria_framework_client::rpc::ClientError;
use asteria_music::app::{MusicApp, Screen};
use asteria_music::library::TrackList;
use asteria_music::player::Command;
use asteria_music_api::{AlbumId, ArtistId, PlaylistId};
use corundum_framework::egui;

use support::*;

fn album_ids(app: &MusicApp) -> Vec<String> {
    app.connection()
        .map(|c| {
            c.library()
                .albums()
                .iter()
                .map(|a| a.id.0.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn titles(entries: &[asteria_music::player::QueueEntry]) -> Vec<String> {
    entries.iter().map(|e| e.title.clone()).collect()
}

fn open_playlist(ctx: &egui::Context, app: &mut MusicApp) {
    run_until(ctx, app, "the playlist list", |a| {
        !a.connection().unwrap().library().playlists().is_empty()
    });
    app.open_playlist(PlaylistId("pl".into()));
    run_until(ctx, app, "the playlist", |a| {
        a.connection().unwrap().library().playlist().is_some()
    });
}

#[test]
fn albums_load_and_the_next_page_follows() {
    let (addr, seen) = Music {
        albums: 250,
        ..Default::default()
    }
    .serve();
    let (_fx, ctx, mut app) = start(addr);
    run_until(&ctx, &mut app, "the first page", |a| {
        album_ids(a).len() == 200
    });
    assert!(app.connection().unwrap().library().has_more_albums());
    app.load_more_albums();
    run_until(&ctx, &mut app, "the second page", |a| {
        album_ids(a).len() == 250
    });
    assert!(!app.connection().unwrap().library().has_more_albums());
    let lists = calls(&seen, "album.list");
    assert_eq!(lists.len(), 2);
    assert_eq!(lists[0]["limit"], 200);
    assert!(lists[0]["cursor"].is_null());
    assert_eq!(lists[1]["cursor"], "200");
    let ids = album_ids(&app);
    assert_eq!(ids[0], "al0");
    assert_eq!(ids[249], "al249");
}

#[test]
fn opening_an_album_shows_its_tracks() {
    let (addr, seen) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    run_until(&ctx, &mut app, "albums", |a| !album_ids(a).is_empty());
    app.open_album(AlbumId("al1".into()));
    run_until(&ctx, &mut app, "the album", |a| {
        a.connection().unwrap().library().album().is_some()
    });
    let lib = app.connection().unwrap().library();
    let album = lib.album().unwrap();
    assert_eq!(album.summary.id.0, "al1");
    assert_eq!(album.tracks.len(), 4);
    assert_eq!(calls(&seen, "album.get")[0]["id"], "al1");
    app.close_album();
    assert!(app.connection().unwrap().library().album().is_none());
}

#[test]
fn clicking_a_track_replaces_the_queue_from_it() {
    let (addr, _) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    app.open_album(AlbumId("al1".into()));
    run_until(&ctx, &mut app, "the album", |a| {
        a.connection().unwrap().library().album().is_some()
    });
    let lib = app.connection().unwrap().library();
    let Some(Command::Replace { entries, start }) = lib.play_from(TrackList::Album, 2) else {
        panic!("expected a Replace");
    };
    assert_eq!(start, 2);
    assert_eq!(titles(&entries), ["al1-t0", "al1-t1", "al1-t2", "al1-t3"]);
    assert!(lib.play_from(TrackList::Album, 4).is_none());
}

#[test]
fn playlist_missing_entries_are_left_out_of_the_queue() {
    let (addr, _) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    open_playlist(&ctx, &mut app);
    let lib = app.connection().unwrap().library();
    // [found, missing, moved, found]: entry 2 is the second of the three playable ones.
    let Some(Command::Replace { entries, start }) = lib.play_from(TrackList::Playlist, 2) else {
        panic!("expected a Replace");
    };
    assert_eq!(titles(&entries), ["pa", "pm", "pd"]);
    assert_eq!(start, 1);
    assert!(lib.play_from(TrackList::Playlist, 1).is_none());
    assert!(lib.play_next(TrackList::Playlist, 1).is_none());
    assert!(lib.append(TrackList::Playlist, 1).is_none());
    assert!(lib.play_from(TrackList::Playlist, 9).is_none());
}

#[test]
fn play_next_and_append_build_their_commands() {
    let (addr, _) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    app.open_album(AlbumId("al1".into()));
    run_until(&ctx, &mut app, "the album", |a| {
        a.connection().unwrap().library().album().is_some()
    });
    let lib = app.connection().unwrap().library();
    let Some(Command::PlayNext(next)) = lib.play_next(TrackList::Album, 3) else {
        panic!("expected PlayNext");
    };
    assert_eq!(titles(&next), ["al1-t3"]);
    let Some(Command::Append(add)) = lib.append(TrackList::Album, 0) else {
        panic!("expected Append");
    };
    assert_eq!(titles(&add), ["al1-t0"]);
    // The artists and duration reach the queue entry.
    assert_eq!(add[0].artist, "Band");
    assert_eq!(add[0].duration_ms, Some(61_000));
}

#[test]
fn an_artist_shows_own_and_appears_on_albums() {
    let (addr, seen) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    run_until(&ctx, &mut app, "artists", |a| {
        a.connection().unwrap().library().artists().len() == 2
    });
    app.open_artist(ArtistId("band".into()));
    run_until(&ctx, &mut app, "the artist", |a| {
        a.connection().unwrap().library().artist().is_some()
    });
    let artist = app.connection().unwrap().library().artist().unwrap();
    assert_eq!(artist.albums.len(), 2);
    assert_eq!(artist.appears_on.len(), 1);
    assert_eq!(calls(&seen, "artist.get")[0]["id"], "band");
    // An album opened from the artist is the artist screen's own.
    app.open_artist_album(AlbumId("own1".into()));
    run_until(&ctx, &mut app, "the artist's album", |a| {
        a.connection().unwrap().library().artist_album().is_some()
    });
    let lib = app.connection().unwrap().library();
    assert!(lib.album().is_none());
    assert!(lib.play_from(TrackList::ArtistAlbum, 0).is_some());
    assert!(lib.play_from(TrackList::Album, 0).is_none());
}

#[test]
fn a_reply_after_switching_servers_is_ignored() {
    let (slow, _) = Music {
        albums: 3,
        prefix: "slow",
        album_delay: Duration::from_millis(600),
        ..Default::default()
    }
    .serve();
    let (fast, _) = Music {
        albums: 2,
        prefix: "fast",
        ..Default::default()
    }
    .serve();
    let (fx, ids) = fixture(&[("slow", slow), ("fast", fast)], 0);
    let ctx = egui::Context::default();
    let mut app = MusicApp::new(fx.paths.clone(), &ctx, player(&ctx));
    frame(&ctx, &mut app);
    app.switch_to(ids[1]);
    run_until(&ctx, &mut app, "the fast server's albums", |a| {
        album_ids(a).len() == 2
    });
    // Long enough for the slow server's answer to have been sent.
    let until = std::time::Instant::now() + Duration::from_millis(900);
    while std::time::Instant::now() < until {
        frame(&ctx, &mut app);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(album_ids(&app), ["fast0", "fast1"]);
}

#[test]
fn a_value_that_does_not_parse_is_a_protocol_error() {
    let (addr, _) = Music {
        bad_albums: true,
        ..Default::default()
    }
    .serve();
    let (_fx, ctx, mut app) = start(addr);
    run_until(&ctx, &mut app, "the error", |a| {
        a.connection().unwrap().library().error().is_some()
    });
    let lib = app.connection().unwrap().library();
    assert!(matches!(lib.error(), Some(ClientError::Protocol(_))));
    assert!(lib.albums().is_empty());
}

#[test]
fn every_frame_draws_without_panicking_on_each_screen() {
    let (addr, _) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    run_until(&ctx, &mut app, "albums", |a| !album_ids(a).is_empty());
    open_playlist(&ctx, &mut app);
    app.open_album(AlbumId("al1".into()));
    app.open_artist(ArtistId("band".into()));
    app.open_artist_album(AlbumId("own1".into()));
    run_until(&ctx, &mut app, "the details", |a| {
        let l = a.connection().unwrap().library();
        l.album().is_some() && l.artist().is_some() && l.artist_album().is_some()
    });
    let screens = [
        Screen::Albums,
        Screen::Artists,
        Screen::Playlists,
        Screen::Queue,
        Screen::Settings,
    ];
    for size in [egui::vec2(1000.0, 700.0), egui::vec2(380.0, 700.0)] {
        for screen in screens {
            app.open_screen(screen);
            for _ in 0..3 {
                frame_sized(&ctx, &mut app, size);
            }
            assert_eq!(app.current_screen(), Some(screen));
        }
    }
}

#[test]
fn a_failed_next_page_is_reported_and_retried() {
    let music = Music {
        albums: 250,
        ..Default::default()
    };
    music
        .fail_later_pages
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (addr, seen) = music.serve();
    let (_fx, ctx, mut app) = start(addr);
    run_until(&ctx, &mut app, "the first page", |a| {
        album_ids(a).len() == 200
    });
    app.load_more_albums();
    run_until(&ctx, &mut app, "the error", |a| {
        a.connection().unwrap().library().error().is_some()
    });
    assert_eq!(album_ids(&app).len(), 200);
    // The error stays (and the grid still draws) until the user retries.
    app.load_more_albums();
    frame(&ctx, &mut app);
    assert_eq!(calls(&seen, "album.list").len(), 2);
    app.retry_albums();
    run_until(&ctx, &mut app, "the second page", |a| {
        album_ids(a).len() == 250
    });
    assert!(app.connection().unwrap().library().error().is_none());
}
