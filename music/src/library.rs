//! What the browse screens show of one server's library, and the requests that fetch it.
//!
//! A [`LibraryState`] belongs to one server: the app keeps it beside that server's
//! [`Session`] in a [`Connection`](crate::connection::Connection) and drops both together on
//! a switch, so nothing the old server sends can reach the new screen. Replies are matched by
//! the [`RequestId`] the state is waiting for; any other reply (an older page, a listing
//! asked again since) is ignored.
//!
//! It holds the three lists (`album.list`, `artist.list`, `playlist.list`, [`PAGE_SIZE`] at a
//! time, the next page once the drawn rows near the end) and, per screen, what is open: an
//! album, an artist and the album opened from it, a playlist. The methods that start playing
//! only build a [`Command`]; the app sends it to the player.

use std::fmt::Debug;

use asteria_framework_client::errors::{error_detail, error_text};
use asteria_framework_client::images::ImageLoader;
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::{Reply, Request, RequestId, Response, Session};
use asteria_music_api::{
    ALBUM_GET, ALBUM_LIST, ARTIST_GET, ARTIST_LIST, Album, AlbumId, AlbumListing, AlbumSummary,
    Artist, ArtistId, ArtistListing, ArtistSummary, EntryStatus, ListParams, MusicTrack,
    PLAYLIST_GET, PLAYLIST_LIST, Playlist, PlaylistId, PlaylistListing, PlaylistSummary,
};
use corundum_framework::egui;
use corundum_framework::gui::{icons, tr};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::cover_grid::{CoverGrid, paint_cover};
use crate::player::{Command, QueueEntry};
use crate::tracks::{self, TrackAction};

/// Items asked for per `*.list` page.
pub const PAGE_SIZE: u32 = 200;
/// The next page of a list of rows is asked for once the drawn rows come within this many of
/// the end (for the cover grid, this many grid rows).
const PREFETCH_ROWS: usize = 3;

/// The list of tracks a row belongs to: which opened thing `play_from` and the others read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackList {
    /// The album opened on the Albums screen.
    Album,
    /// The album opened from an artist.
    ArtistAlbum,
    /// The playlist opened on the Playlists screen.
    Playlist,
}

/// Which page an in-flight `*.list` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageKind {
    /// The listing from the start: it replaces what is shown.
    First,
    /// The page after the last one: it is appended.
    Next,
}

/// A `*.list` result: its items and the cursor of the page after.
trait Listing: DeserializeOwned {
    type Item: Debug;
    fn into_parts(self) -> (Vec<Self::Item>, Option<String>);
}

impl Listing for AlbumListing {
    type Item = AlbumSummary;
    fn into_parts(self) -> (Vec<AlbumSummary>, Option<String>) {
        (self.albums, self.next_cursor)
    }
}

impl Listing for ArtistListing {
    type Item = ArtistSummary;
    fn into_parts(self) -> (Vec<ArtistSummary>, Option<String>) {
        (self.artists, self.next_cursor)
    }
}

impl Listing for PlaylistListing {
    type Item = PlaylistSummary;
    fn into_parts(self) -> (Vec<PlaylistSummary>, Option<String>) {
        (self.playlists, self.next_cursor)
    }
}

/// Reads the value of a reply that waited for `method`. A result that is not the value of
/// a call, or does not read as `T`, is a [`ClientError::Protocol`].
fn decode<T: DeserializeOwned>(
    method: &str,
    result: &Result<Response, ClientError>,
) -> Result<T, ClientError> {
    match result {
        Ok(Response::Value(v)) => {
            T::deserialize(v).map_err(|e| ClientError::Protocol(format!("{method}: {e}")))
        }
        Ok(other) => Err(ClientError::Protocol(format!(
            "{method} answered with {other:?}"
        ))),
        Err(e) => Err(e.clone()),
    }
}

fn call(session: &mut Session, method: &str, params: impl Serialize) -> RequestId {
    session.request(Request::Call {
        method: method.to_string(),
        params: serde_json::to_value(params).unwrap_or_default(),
    })
}

/// One paged list.
#[derive(Debug)]
struct Paged<L: Listing> {
    method: &'static str,
    items: Vec<L::Item>,
    next_cursor: Option<String>,
    /// The `*.list` waited for. Only one is in flight at a time.
    listing: Option<(RequestId, PageKind)>,
    /// Why the last one failed, and which page it was; cleared by the next success.
    error: Option<(ClientError, PageKind)>,
}

impl<L: Listing> Paged<L> {
    fn new(method: &'static str) -> Self {
        Paged {
            method,
            items: Vec::new(),
            next_cursor: None,
            listing: None,
            error: None,
        }
    }

    fn request(&mut self, session: &mut Session, cursor: Option<String>, kind: PageKind) {
        let params = ListParams {
            cursor,
            limit: Some(PAGE_SIZE),
        };
        // A page asked earlier is now forgotten: its reply will not match.
        self.listing = Some((call(session, self.method, params), kind));
    }

    /// Fetch from the start; what is shown stays until the page arrives.
    fn reload(&mut self, session: &mut Session) {
        self.error = None;
        self.request(session, None, PageKind::First);
    }

    /// The next page, unless one is on its way, there is none, or the last one failed.
    fn load_more(&mut self, session: &mut Session) {
        if self.listing.is_some() || self.error.is_some() {
            return;
        }
        if let Some(cursor) = self.next_cursor.clone() {
            self.request(session, Some(cursor), PageKind::Next);
        }
    }

    fn retry(&mut self, session: &mut Session) {
        match self.error.take() {
            Some((_, PageKind::Next)) => self.load_more(session),
            Some((_, PageKind::First)) => self.reload(session),
            None => {}
        }
    }

    fn handle(&mut self, reply: &Reply) -> bool {
        let Some((id, kind)) = self.listing else {
            return false;
        };
        if id != reply.id {
            return false;
        }
        self.listing = None;
        match decode::<L>(self.method, &reply.result) {
            Ok(listing) => {
                let (items, next) = listing.into_parts();
                match kind {
                    PageKind::First => self.items = items,
                    PageKind::Next => self.items.extend(items),
                }
                self.next_cursor = next;
                self.error = None;
            }
            Err(e) => self.error = Some((e, kind)),
        }
        true
    }
}

/// One opened album, artist or playlist: asked for by id, shown once it arrives.
#[derive(Debug)]
struct Detail<I, T> {
    id: I,
    method: &'static str,
    request: Option<RequestId>,
    value: Option<T>,
    error: Option<ClientError>,
}

impl<I: Serialize, T: DeserializeOwned> Detail<I, T> {
    fn open(session: &mut Session, method: &'static str, id: I) -> Self {
        let mut d = Detail {
            id,
            method,
            request: None,
            value: None,
            error: None,
        };
        d.fetch(session);
        d
    }

    /// Asks again; what is shown stays until the answer comes.
    fn fetch(&mut self, session: &mut Session) {
        self.error = None;
        self.request = Some(call(
            session,
            self.method,
            serde_json::json!({ "id": self.id }),
        ));
    }

    fn handle(&mut self, reply: &Reply) -> bool {
        if self.request != Some(reply.id) {
            return false;
        }
        self.request = None;
        match decode::<T>(self.method, &reply.result) {
            Ok(v) => {
                self.value = Some(v);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        true
    }
}

/// One server's lists and what is open of them.
#[derive(Debug)]
pub struct LibraryState {
    albums: Paged<AlbumListing>,
    artists: Paged<ArtistListing>,
    playlists: Paged<PlaylistListing>,
    album: Option<Detail<AlbumId, Album>>,
    artist: Option<Detail<ArtistId, Artist>>,
    artist_album: Option<Detail<AlbumId, Album>>,
    playlist: Option<Detail<PlaylistId, Playlist>>,
}

impl Default for LibraryState {
    fn default() -> Self {
        LibraryState::new()
    }
}

impl LibraryState {
    /// Nothing loaded or asked for yet.
    pub fn new() -> LibraryState {
        LibraryState {
            albums: Paged::new(ALBUM_LIST),
            artists: Paged::new(ARTIST_LIST),
            playlists: Paged::new(PLAYLIST_LIST),
            album: None,
            artist: None,
            artist_album: None,
            playlist: None,
        }
    }

    /// Asks for the first page of each list. The requests queue behind the handshake.
    pub fn start(&mut self, session: &mut Session) {
        self.reload(session);
    }

    /// Asks for everything shown again, from the start; what is shown stays until it arrives.
    pub fn reload(&mut self, session: &mut Session) {
        self.albums.reload(session);
        self.artists.reload(session);
        self.playlists.reload(session);
        if let Some(d) = &mut self.album {
            d.fetch(session);
        }
        if let Some(d) = &mut self.artist {
            d.fetch(session);
        }
        if let Some(d) = &mut self.artist_album {
            d.fetch(session);
        }
        if let Some(d) = &mut self.playlist {
            d.fetch(session);
        }
    }

    /// Take `reply` if it is one this state waits for.
    pub fn handle(&mut self, reply: &Reply) {
        let _ = self.albums.handle(reply)
            || self.artists.handle(reply)
            || self.playlists.handle(reply)
            || self.album.as_mut().is_some_and(|d| d.handle(reply))
            || self.artist.as_mut().is_some_and(|d| d.handle(reply))
            || self.artist_album.as_mut().is_some_and(|d| d.handle(reply))
            || self.playlist.as_mut().is_some_and(|d| d.handle(reply));
    }

    // The lists.

    pub fn albums(&self) -> &[AlbumSummary] {
        &self.albums.items
    }

    pub fn artists(&self) -> &[ArtistSummary] {
        &self.artists.items
    }

    pub fn playlists(&self) -> &[PlaylistSummary] {
        &self.playlists.items
    }

    pub fn has_more_albums(&self) -> bool {
        self.albums.next_cursor.is_some()
    }

    pub fn has_more_artists(&self) -> bool {
        self.artists.next_cursor.is_some()
    }

    pub fn has_more_playlists(&self) -> bool {
        self.playlists.next_cursor.is_some()
    }

    /// Fetch the next page of albums, unless one is on its way, there is none, or the last
    /// one failed.
    pub fn load_more_albums(&mut self, session: &mut Session) {
        self.albums.load_more(session);
    }

    pub fn load_more_artists(&mut self, session: &mut Session) {
        self.artists.load_more(session);
    }

    pub fn load_more_playlists(&mut self, session: &mut Session) {
        self.playlists.load_more(session);
    }

    /// Why a list could not be loaded (the first that failed).
    pub fn error(&self) -> Option<&ClientError> {
        self.albums
            .error
            .as_ref()
            .or(self.artists.error.as_ref())
            .or(self.playlists.error.as_ref())
            .map(|(e, _)| e)
    }

    // What is open.

    pub fn album(&self) -> Option<&Album> {
        self.album.as_ref()?.value.as_ref()
    }

    pub fn artist(&self) -> Option<&Artist> {
        self.artist.as_ref()?.value.as_ref()
    }

    /// The album opened from the artist shown.
    pub fn artist_album(&self) -> Option<&Album> {
        self.artist_album.as_ref()?.value.as_ref()
    }

    pub fn playlist(&self) -> Option<&Playlist> {
        self.playlist.as_ref()?.value.as_ref()
    }

    pub fn open_album(&mut self, session: &mut Session, id: AlbumId) {
        self.album = Some(Detail::open(session, ALBUM_GET, id));
    }

    pub fn open_artist(&mut self, session: &mut Session, id: ArtistId) {
        self.artist = Some(Detail::open(session, ARTIST_GET, id));
        self.artist_album = None;
    }

    pub fn open_artist_album(&mut self, session: &mut Session, id: AlbumId) {
        self.artist_album = Some(Detail::open(session, ALBUM_GET, id));
    }

    pub fn open_playlist(&mut self, session: &mut Session, id: PlaylistId) {
        self.playlist = Some(Detail::open(session, PLAYLIST_GET, id));
    }

    pub fn close_album(&mut self) {
        self.album = None;
    }

    pub fn close_artist(&mut self) {
        self.artist = None;
        self.artist_album = None;
    }

    pub fn close_artist_album(&mut self) {
        self.artist_album = None;
    }

    pub fn close_playlist(&mut self) {
        self.playlist = None;
    }

    // Playing.

    /// The playable tracks of `list`, each with the index of its row (an album's track, a
    /// playlist's entry). A playlist's `Missing` entries are not playable.
    fn playable(&self, list: TrackList) -> Vec<(usize, &MusicTrack)> {
        match list {
            TrackList::Album => album_tracks(self.album()),
            TrackList::ArtistAlbum => album_tracks(self.artist_album()),
            TrackList::Playlist => self
                .playlist()
                .map(|p| {
                    p.entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.status != EntryStatus::Missing)
                        .filter_map(|(i, e)| Some((i, e.track.as_ref()?)))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// The command that replaces the queue with the playable tracks of `list` and starts at
    /// the row `index`. `None` if there is no such row or it cannot be played.
    pub fn play_from(&self, list: TrackList, index: usize) -> Option<Command> {
        let tracks = self.playable(list);
        let start = tracks.iter().position(|(i, _)| *i == index)?;
        Some(Command::Replace {
            entries: tracks
                .into_iter()
                .map(|(_, t)| QueueEntry::from_track(t))
                .collect(),
            start,
        })
    }

    fn one(&self, list: TrackList, index: usize) -> Option<Vec<QueueEntry>> {
        let tracks = self.playable(list);
        let (_, t) = tracks.into_iter().find(|(i, _)| *i == index)?;
        Some(vec![QueueEntry::from_track(t)])
    }

    /// The command that plays the track at the row `index` of `list` right after the current one.
    pub fn play_next(&self, list: TrackList, index: usize) -> Option<Command> {
        self.one(list, index).map(Command::PlayNext)
    }

    /// The command that adds the track at the row `index` of `list` to the end of the queue.
    pub fn append(&self, list: TrackList, index: usize) -> Option<Command> {
        self.one(list, index).map(Command::Append)
    }

    fn command(&self, list: TrackList, action: TrackAction) -> Option<Command> {
        match action {
            TrackAction::Play(i) => self.play_from(list, i),
            TrackAction::PlayNext(i) => self.play_next(list, i),
            TrackAction::Append(i) => self.append(list, i),
        }
    }

    // The screens.

    /// The Albums screen: the cover grid, or the album opened. Returns what to play.
    pub fn albums_ui(&mut self, ui: &mut egui::Ui, session: &mut Session) -> Option<Command> {
        if let Some(detail) = &self.album {
            let out = album_page_ui(ui, detail, session.images());
            if out.retry
                && let Some(d) = &mut self.album
            {
                d.fetch(session);
            }
            let command = out.action.and_then(|a| self.command(TrackList::Album, a));
            if out.back {
                self.close_album();
            }
            return command;
        }
        if self.albums.items.is_empty() {
            if list_problem_ui(ui, &self.albums, "asteria-music-albums-empty") {
                self.albums.retry(session);
            }
            return None;
        }
        let r = CoverGrid::new(&self.albums.items)
            .id_salt(("asteria_music_albums", session.generation()))
            .show(ui, session.images());
        if r.last_visible
            .is_some_and(|last| last + PREFETCH_ROWS * r.columns >= self.albums.items.len())
        {
            self.albums.load_more(session);
        }
        if let Some(id) = r.opened {
            self.open_album(session, id);
        }
        None
    }

    /// The Artists screen: the artists, or the artist opened with its albums (and one of
    /// them opened).
    pub fn artists_ui(&mut self, ui: &mut egui::Ui, session: &mut Session) -> Option<Command> {
        if let Some(detail) = &self.artist_album {
            let out = album_page_ui(ui, detail, session.images());
            if out.retry
                && let Some(d) = &mut self.artist_album
            {
                d.fetch(session);
            }
            let command = out
                .action
                .and_then(|a| self.command(TrackList::ArtistAlbum, a));
            if out.back {
                self.close_artist_album();
            }
            return command;
        }
        if let Some(detail) = &self.artist {
            let mut back = false;
            if ui.button(tr!(ui, "asteria-music-detail-back")).clicked() {
                back = true;
            }
            let mut opened = None;
            match &detail.value {
                None => {
                    if problem_ui(ui, detail.request.is_some(), detail.error.as_ref())
                        && let Some(d) = &mut self.artist
                    {
                        d.fetch(session);
                    }
                }
                Some(artist) => {
                    egui::ScrollArea::vertical()
                        .id_salt("asteria_music_artist")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.heading(artist.summary.name.as_str());
                            if !artist.albums.is_empty() {
                                ui.label(tr!(ui, "asteria-music-detail-own-albums"));
                                opened = CoverGrid::new(&artist.albums)
                                    .show_flowing(ui, session.images());
                            }
                            if !artist.appears_on.is_empty() {
                                ui.add_space(8.0);
                                ui.label(tr!(ui, "asteria-music-detail-appears-on"));
                                opened = CoverGrid::new(&artist.appears_on)
                                    .show_flowing(ui, session.images())
                                    .or(opened.take());
                            }
                        });
                }
            }
            if let Some(id) = opened {
                self.open_artist_album(session, id);
            }
            if back {
                self.close_artist();
            }
            return None;
        }
        if self.artists.items.is_empty() {
            if list_problem_ui(ui, &self.artists, "asteria-music-artists-empty") {
                self.artists.retry(session);
            }
            return None;
        }
        let mut opened = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
        let total = self.artists.items.len();
        let mut last = 0;
        egui::ScrollArea::vertical()
            .id_salt(("asteria_music_artists", session.generation()))
            .auto_shrink([false, false])
            .show_rows(ui, row_h, total, |ui, range| {
                last = range.end.saturating_sub(1);
                for artist in &self.artists.items[range] {
                    ui.horizontal(|ui| {
                        if ui.link(artist.name.as_str()).clicked() {
                            opened = Some(artist.id.clone());
                        }
                        ui.weak(tr!(
                            ui,
                            "asteria-music-artists-albums",
                            count = u64::from(artist.album_count)
                        ));
                    });
                }
            });
        if last + PREFETCH_ROWS >= total {
            self.artists.load_more(session);
        }
        if let Some(id) = opened {
            self.open_artist(session, id);
        }
        None
    }

    /// The Playlists screen: the playlists, or the playlist opened with its tracks.
    pub fn playlists_ui(&mut self, ui: &mut egui::Ui, session: &mut Session) -> Option<Command> {
        if let Some(detail) = &self.playlist {
            let mut back = false;
            let mut action = None;
            if ui.button(tr!(ui, "asteria-music-detail-back")).clicked() {
                back = true;
            }
            match &detail.value {
                None => {
                    if problem_ui(ui, detail.request.is_some(), detail.error.as_ref())
                        && let Some(d) = &mut self.playlist
                    {
                        d.fetch(session);
                    }
                }
                Some(playlist) => {
                    ui.heading(playlist.summary.name.as_str());
                    if playlist.truncated {
                        ui.weak(tr!(ui, "asteria-music-detail-truncated"));
                    }
                    let rows = tracks::playlist_rows(playlist);
                    action = tracks::show(
                        ui,
                        ("asteria_music_playlist", playlist.summary.id.0.as_str()),
                        &rows,
                        None,
                    );
                }
            }
            let command = action.and_then(|a| self.command(TrackList::Playlist, a));
            if back {
                self.close_playlist();
            }
            return command;
        }
        if self.playlists.items.is_empty() {
            if list_problem_ui(ui, &self.playlists, "asteria-music-playlists-empty") {
                self.playlists.retry(session);
            }
            return None;
        }
        let mut opened = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
        let total = self.playlists.items.len();
        let mut last = 0;
        egui::ScrollArea::vertical()
            .id_salt(("asteria_music_playlists", session.generation()))
            .auto_shrink([false, false])
            .show_rows(ui, row_h, total, |ui, range| {
                last = range.end.saturating_sub(1);
                for playlist in &self.playlists.items[range] {
                    ui.horizontal(|ui| {
                        ui.add(icons::image(ui, icons::file_text(), 14.0));
                        if ui.link(playlist.name.as_str()).clicked() {
                            opened = Some(playlist.id.clone());
                        }
                    });
                }
            });
        if last + PREFETCH_ROWS >= total {
            self.playlists.load_more(session);
        }
        if let Some(id) = opened {
            self.open_playlist(session, id);
        }
        None
    }
}

fn album_tracks(album: Option<&Album>) -> Vec<(usize, &MusicTrack)> {
    album
        .map(|a| a.tracks.iter().enumerate().collect())
        .unwrap_or_default()
}

/// What the user did on an opened album's page.
struct AlbumPageOut {
    back: bool,
    retry: bool,
    action: Option<TrackAction>,
}

fn album_page_ui(
    ui: &mut egui::Ui,
    detail: &Detail<AlbumId, Album>,
    images: &mut ImageLoader,
) -> AlbumPageOut {
    let mut out = AlbumPageOut {
        back: false,
        retry: false,
        action: None,
    };
    if ui.button(tr!(ui, "asteria-music-detail-back")).clicked() {
        out.back = true;
    }
    let Some(album) = &detail.value else {
        out.retry = problem_ui(ui, detail.request.is_some(), detail.error.as_ref());
        return out;
    };
    let summary = &album.summary;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 96.0), egui::Sense::hover());
        paint_cover(ui, rect, summary, images);
        ui.vertical(|ui| {
            ui.heading(summary.title.as_str());
            let line = match summary.year {
                Some(year) => format!("{} ({year})", summary.album_artist),
                None => summary.album_artist.clone(),
            };
            ui.label(line);
            ui.weak(tr!(
                ui,
                "asteria-music-detail-album-tracks",
                count = album.tracks.len() as u64
            ));
        });
    });
    ui.add_space(4.0);
    let rows = tracks::album_rows(album);
    out.action = tracks::show(
        ui,
        ("asteria_music_album", summary.id.0.as_str()),
        &rows,
        Some(summary.album_artist.as_str()),
    );
    out
}

/// A word about a request that is on its way or failed. Returns whether "try again" was clicked.
fn problem_ui(ui: &mut egui::Ui, loading: bool, error: Option<&ClientError>) -> bool {
    let mut retry = false;
    if let Some(e) = error {
        ui.horizontal_wrapped(|ui| {
            let reason = error_text(ui.ctx(), e);
            let label = ui.colored_label(
                ui.visuals().error_fg_color,
                tr!(ui, "asteria-music-library-failed", reason = reason),
            );
            if let Some(detail) = error_detail(e) {
                label.on_hover_text(detail);
            }
            retry = ui.button(tr!(ui, "asteria-music-library-retry")).clicked();
        });
    } else if loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(tr!(ui, "asteria-music-library-loading"));
        });
    }
    retry
}

/// A list with nothing in it: loading, failed, or empty (`empty` is the string for that).
fn list_problem_ui<L: Listing>(ui: &mut egui::Ui, list: &Paged<L>, empty: &'static str) -> bool {
    let error = list.error.as_ref().map(|(e, _)| e);
    if error.is_none() && list.listing.is_none() {
        ui.weak(tr!(ui, empty));
        return false;
    }
    problem_ui(ui, list.listing.is_some(), error)
}
