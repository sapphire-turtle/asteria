//! The browse screens against a stub music server: paging, opening, what playing builds,
//! and what a stale or broken reply does.

mod support;

use std::time::Duration;

use asteria_common_client::rpc::ClientError;
use asteria_common_client::session::Session;
use asteria_music_api::{AlbumId, ArtistId, PlaylistId};
use asteria_music_gui::app::{MusicApp, Screen};
use asteria_music_gui::library::{LibraryState, TrackList};
use asteria_music_gui::player::Command;
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

fn titles(entries: &[asteria_music_gui::player::QueueEntry]) -> Vec<String> {
    entries.iter().map(|e| e.title.clone()).collect()
}

/// The connection's session and library, as the play methods take them.
fn parts(app: &MusicApp) -> (&Session, &LibraryState) {
    let c = app.connection().unwrap();
    (c.session(), c.library())
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
    let (session, lib) = parts(&app);
    let Some(Command::Replace {
        items: entries,
        start,
    }) = lib.play_from(session, TrackList::Album, 2)
    else {
        panic!("expected a Replace");
    };
    assert_eq!(start, 2);
    assert_eq!(titles(&entries), ["al1-t0", "al1-t1", "al1-t2", "al1-t3"]);
    assert!(lib.play_from(session, TrackList::Album, 4).is_none());
}

#[test]
fn playlist_missing_entries_are_left_out_of_the_queue() {
    let (addr, _) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    open_playlist(&ctx, &mut app);
    let (session, lib) = parts(&app);
    // [found, missing, moved, found]: entry 2 is the second of the three playable ones.
    let Some(Command::Replace {
        items: entries,
        start,
    }) = lib.play_from(session, TrackList::Playlist, 2)
    else {
        panic!("expected a Replace");
    };
    assert_eq!(titles(&entries), ["pa", "pm", "pd"]);
    assert_eq!(start, 1);
    assert!(lib.play_from(session, TrackList::Playlist, 1).is_none());
    assert!(lib.play_next(session, TrackList::Playlist, 1).is_none());
    assert!(lib.append(session, TrackList::Playlist, 1).is_none());
    assert!(lib.play_from(session, TrackList::Playlist, 9).is_none());
}

#[test]
fn play_next_and_append_build_their_commands() {
    let (addr, _) = Music::default().serve();
    let (_fx, ctx, mut app) = start(addr);
    app.open_album(AlbumId("al1".into()));
    run_until(&ctx, &mut app, "the album", |a| {
        a.connection().unwrap().library().album().is_some()
    });
    let (session, lib) = parts(&app);
    let Some(Command::PlayNext(next)) = lib.play_next(session, TrackList::Album, 3) else {
        panic!("expected PlayNext");
    };
    assert_eq!(titles(&next), ["al1-t3"]);
    let Some(Command::Append(add)) = lib.append(session, TrackList::Album, 0) else {
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
    let (session, lib) = parts(&app);
    assert!(lib.album().is_none());
    assert!(lib.play_from(session, TrackList::ArtistAlbum, 0).is_some());
    assert!(lib.play_from(session, TrackList::Album, 0).is_none());
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
    let (player, settings) = player(&ctx);
    let mut app = MusicApp::new(fx.paths.clone(), &ctx, player, settings);
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

const WIDE: egui::Vec2 = egui::vec2(1000.0, 700.0);
const NARROW: egui::Vec2 = egui::vec2(380.0, 700.0);

/// The app on a stub that answers `ops.apply`, with the album `al1` open and drawn at `size`;
/// the player's settings come with it, to wait on.
fn open_album_at(
    size: egui::Vec2,
) -> (
    Fixture,
    egui::Context,
    MusicApp,
    asteria_music_gui::player::OpenerSettings,
    Seen,
) {
    open_album_with(size, |_| {})
}

/// As [`open_album_at`], with the app's paths changed by `paths` first.
fn open_album_with(
    size: egui::Vec2,
    paths: impl FnOnce(&mut asteria_music_gui::app::AppPaths),
) -> (
    Fixture,
    egui::Context,
    MusicApp,
    asteria_music_gui::player::OpenerSettings,
    Seen,
) {
    let (addr, seen) = Music::default().serve();
    let (mut fx, _) = fixture(&[("A", addr)], 0);
    paths(&mut fx.paths);
    let ctx = egui::Context::default();
    let (player, settings, _, _) = player_with_fakes(&ctx);
    let mut app = MusicApp::new(fx.paths.clone(), &ctx, player, settings.clone());
    // Handing the player its endpoint resets it: wait for that before anything is queued.
    run_until(&ctx, &mut app, "the endpoint", |_| {
        settings.endpoint().is_some()
    });
    app.open_album(AlbumId("al1".into()));
    run_until(&ctx, &mut app, "the album", |a| {
        a.connection().unwrap().library().album().is_some()
    });
    for _ in 0..3 {
        frame_sized(&ctx, &mut app, size);
    }
    (fx, ctx, app, settings, seen)
}

/// The rating ops the stub received, as (item, value).
fn rated(seen: &Seen) -> Vec<(String, i64)> {
    calls(seen, "ops.apply")
        .iter()
        .flat_map(|p| p["ops"].as_array().unwrap().clone())
        .map(|op| {
            (
                op["item"].as_str().unwrap().to_string(),
                op["value"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn wait_rated(seen: &Seen, want: (&str, i64)) {
    wait_until("the rating to reach the server", || {
        rated(seen).contains(&(want.0.to_string(), want.1))
    });
}

#[test]
fn a_star_in_a_row_rates_without_playing() {
    let (_fx, ctx, mut app, _, seen) = open_album_at(WIDE);
    // Nothing plays, so the side pane has no stars: these are the rows', 5 to a row.
    let stars = star_centres(&ctx, &mut app, WIDE);
    assert_eq!(stars.len(), 4 * 5, "{stars:?}");
    click_at(&ctx, &mut app, WIDE, stars[2], egui::PointerButton::Primary);
    frame_sized(&ctx, &mut app, WIDE);
    wait_rated(&seen, ("al1-t0", 3));
    assert_eq!(rated(&seen), [("al1-t0".to_string(), 3)]);
    // Long enough for a play command to have reached the player.
    for _ in 0..10 {
        frame_sized(&ctx, &mut app, WIDE);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.player().state().queue.is_empty());
    assert_eq!(app.notice(), None);
}

#[test]
fn narrow_rows_rate_from_the_menu() {
    let (_fx, ctx, mut app, _, seen) = open_album_at(NARROW);
    assert!(star_centres(&ctx, &mut app, NARROW).is_empty());
    let row = widget_centre(&ctx, &mut app, NARROW, "al1-t0");
    click_at(&ctx, &mut app, NARROW, row, egui::PointerButton::Secondary);
    frame_sized(&ctx, &mut app, NARROW);
    let menu = widget_centre(&ctx, &mut app, NARROW, "Rating");
    click_at(&ctx, &mut app, NARROW, menu, egui::PointerButton::Primary);
    frame_sized(&ctx, &mut app, NARROW);
    let two = widget_centre(&ctx, &mut app, NARROW, "★★");
    click_at(&ctx, &mut app, NARROW, two, egui::PointerButton::Primary);
    frame_sized(&ctx, &mut app, NARROW);
    wait_rated(&seen, ("al1-t0", 2));
    assert_eq!(rated(&seen), [("al1-t0".to_string(), 2)]);
    // The choice closes the menu.
    assert!(widgets(&ctx, &mut app, NARROW, |l| l == "Rating").is_empty());
}

#[test]
fn the_side_pane_rates_the_current_track() {
    let (_fx, ctx, mut app, _, seen) = open_album_at(WIDE);
    app.play_from(TrackList::Album, 0);
    run_until(&ctx, &mut app, "the track to be current", |a| {
        a.player().state().current.is_some()
    });
    // The queue has no stars of its own: these are the side pane's.
    app.open_screen(Screen::Queue);
    frame_sized(&ctx, &mut app, WIDE);
    let stars = star_centres(&ctx, &mut app, WIDE);
    assert_eq!(stars.len(), 5, "{stars:?}");
    click_at(&ctx, &mut app, WIDE, stars[4], egui::PointerButton::Primary);
    frame_sized(&ctx, &mut app, WIDE);
    wait_rated(&seen, ("al1-t0", 5));
    assert_eq!(rated(&seen), [("al1-t0".to_string(), 5)]);
}

#[test]
fn a_rating_with_nowhere_to_keep_it_is_told() {
    // Without a data directory there is no queue to keep a rating in.
    let (_fx, ctx, mut app, _, seen) = open_album_with(WIDE, |p| p.data = None);
    let stars = star_centres(&ctx, &mut app, WIDE);
    click_at(&ctx, &mut app, WIDE, stars[0], egui::PointerButton::Primary);
    frame_sized(&ctx, &mut app, WIDE);
    assert_eq!(app.notice(), Some("Could not save a rating"));
    assert!(rated(&seen).is_empty());
}

#[test]
fn a_click_between_two_stars_neither_rates_nor_plays() {
    let (_fx, ctx, mut app, _, seen) = open_album_at(WIDE);
    let stars = star_centres(&ctx, &mut app, WIDE);
    // Halfway between the first two stars of row 0: the gap, not a star.
    let gap = egui::pos2((stars[0].x + stars[1].x) / 2.0, stars[0].y);
    click_at(&ctx, &mut app, WIDE, gap, egui::PointerButton::Primary);
    for _ in 0..10 {
        frame_sized(&ctx, &mut app, WIDE);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(app.player().state().queue.is_empty());
    assert!(rated(&seen).is_empty());
}

#[test]
fn narrow_a_notice_is_told_at_the_foot() {
    let (_fx, ctx, mut app, _, _) = open_album_with(NARROW, |p| p.data = None);
    let row = widget_centre(&ctx, &mut app, NARROW, "al1-t0");
    click_at(&ctx, &mut app, NARROW, row, egui::PointerButton::Secondary);
    frame_sized(&ctx, &mut app, NARROW);
    let menu = widget_centre(&ctx, &mut app, NARROW, "Rating");
    click_at(&ctx, &mut app, NARROW, menu, egui::PointerButton::Primary);
    frame_sized(&ctx, &mut app, NARROW);
    let one = widget_centre(&ctx, &mut app, NARROW, "★");
    click_at(&ctx, &mut app, NARROW, one, egui::PointerButton::Primary);
    frame_sized(&ctx, &mut app, NARROW);
    assert_eq!(app.notice(), Some("Could not save a rating"));
    let at: Vec<egui::Pos2> = widgets(&ctx, &mut app, NARROW, |l| l == "Could not save a rating")
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    assert!(!at.is_empty(), "the notice is drawn");
    // Narrow there is no status bar: the notice sits at the foot of the screen.
    assert!(at.iter().all(|p| p.y > 350.0), "{at:?}");
}
