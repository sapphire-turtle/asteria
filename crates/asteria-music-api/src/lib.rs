//! Wire types shared between the asteria-music server and its clients.

use serde::{Deserialize, Serialize};

pub use asteria_framework_api::*;

/// Application identifier sent in the handshake.
pub const APP: &str = "asteria-music";
/// Version of this application's API.
pub const APP_API: u32 = parse_major(env!("CARGO_PKG_VERSION_MAJOR"));

/// The [`ClientInfo`] a client of this application sends.
pub fn client_info() -> ClientInfo {
    ClientInfo {
        app: APP.into(),
        app_api: APP_API,
    }
}

/// Method name of `album.list`.
pub const ALBUM_LIST: &str = "album.list";
/// Method name of `album.get`.
pub const ALBUM_GET: &str = "album.get";
/// Method name of `artist.list`.
pub const ARTIST_LIST: &str = "artist.list";
/// Method name of `artist.get`.
pub const ARTIST_GET: &str = "artist.get";
/// Method name of `playlist.list`.
pub const PLAYLIST_LIST: &str = "playlist.list";
/// Method name of `playlist.get`.
pub const PLAYLIST_GET: &str = "playlist.get";

/// Tags read from a track file. Every field defaults, so a cached value written by an
/// older server still deserializes after fields are added.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackTags {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub artist_sort: Option<String>,
    #[serde(default)]
    pub album_artist_sort: Option<String>,
    #[serde(default)]
    pub album_sort: Option<String>,
    #[serde(default)]
    pub track: Option<u32>,
    #[serde(default)]
    pub track_total: Option<u32>,
    #[serde(default)]
    pub disc: Option<u32>,
    #[serde(default)]
    pub disc_total: Option<u32>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub has_embedded_cover: bool,
    /// Why the tags could not be read; no server paths.
    #[serde(default)]
    pub tag_error: Option<String>,
}

/// One track: its media file, its tags, and the title to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MusicTrack {
    pub item: MediaItem,
    pub tags: TrackTags,
    /// The title tag, or the file stem when there is none.
    pub display_title: String,
}

/// Stable album identifier, opaque to clients.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AlbumId(pub String);

/// Stable artist identifier, opaque to clients.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtistId(pub String);

/// Stable playlist identifier, opaque to clients.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistId(pub String);

/// Paging parameters shared by the `*.list` methods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListParams {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// One album in a listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumSummary {
    pub id: AlbumId,
    pub title: String,
    pub album_artist: String,
    pub album_artist_id: ArtistId,
    pub year: Option<i32>,
    pub track_count: u32,
    /// The item used as the album cover, if any.
    pub cover_item: Option<ItemId>,
}

/// Result of `album.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumListing {
    pub albums: Vec<AlbumSummary>,
    pub next_cursor: Option<String>,
}

/// Parameters of `album.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumGetParams {
    pub id: AlbumId,
}

/// Result of `album.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub summary: AlbumSummary,
    /// Album folder, library-relative.
    pub folder: String,
    pub tracks: Vec<MusicTrack>,
}

/// One artist in a listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtistSummary {
    pub id: ArtistId,
    pub name: String,
    pub album_count: u32,
}

/// Result of `artist.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtistListing {
    pub artists: Vec<ArtistSummary>,
    pub next_cursor: Option<String>,
}

/// Parameters of `artist.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtistGetParams {
    pub id: ArtistId,
}

/// Result of `artist.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artist {
    pub summary: ArtistSummary,
    /// Albums by this artist as album artist.
    pub albums: Vec<AlbumSummary>,
    /// Albums this artist has tracks on but does not album-artist.
    pub appears_on: Vec<AlbumSummary>,
}

/// One playlist in a listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: PlaylistId,
    pub name: String,
    /// Playlist file path, library-relative.
    pub path: String,
}

/// Result of `playlist.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistListing {
    pub playlists: Vec<PlaylistSummary>,
    pub next_cursor: Option<String>,
}

/// Parameters of `playlist.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistGetParams {
    pub id: PlaylistId,
}

/// Whether a playlist entry still resolves to a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// Resolved at its written path.
    Found,
    /// Resolved after the file was moved.
    Moved,
    /// No track could be found for it.
    Missing,
}

/// One line of a playlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    /// The path as written in the playlist file.
    pub raw_path: String,
    pub title_hint: Option<String>,
    pub status: EntryStatus,
    pub track: Option<MusicTrack>,
}

/// Result of `playlist.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub summary: PlaylistSummary,
    pub entries: Vec<PlaylistEntry>,
    /// Set when the entries were cut short.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_api_tracks_crate_major() {
        assert_eq!(super::APP_API, 2);
    }

    #[test]
    fn track_tags_roundtrip_and_defaults() {
        let empty: TrackTags = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, TrackTags::default());
        assert_eq!(empty.title, None);
        assert!(empty.artists.is_empty());
        assert!(!empty.has_embedded_cover);
        assert_eq!(empty.tag_error, None);

        let full = TrackTags {
            title: Some("Song".into()),
            artists: vec!["A".into(), "B".into()],
            album: Some("Album".into()),
            album_artist: Some("A".into()),
            artist_sort: Some("A, The".into()),
            album_artist_sort: Some("A, The".into()),
            album_sort: Some("Album".into()),
            track: Some(3),
            track_total: Some(10),
            disc: Some(1),
            disc_total: Some(2),
            year: Some(2024),
            genres: vec!["Rock".into()],
            duration_ms: Some(215_000),
            has_embedded_cover: true,
            tag_error: Some("unreadable".into()),
        };
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<TrackTags>(&json).unwrap(), full);
    }

    #[test]
    fn entry_status_wire_names() {
        assert_eq!(
            serde_json::to_string(&EntryStatus::Found).unwrap(),
            r#""found""#
        );
        assert_eq!(
            serde_json::to_string(&EntryStatus::Moved).unwrap(),
            r#""moved""#
        );
        assert_eq!(
            serde_json::to_string(&EntryStatus::Missing).unwrap(),
            r#""missing""#
        );
    }
}
