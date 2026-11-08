//! What the music app saves for offline use, and how it answers from the copies.

use asteria_common_client::offline::{Doc, Docs, Fetch, MediaKey, OfflineCatalog, PinTarget};
use asteria_common_client::rpc::ClientError;
use asteria_common_client::session::{Request, Response};
use asteria_music_api::{
    ALBUM_GET, ALBUM_LIST, ARTIST_GET, ARTIST_LIST, Album, AlbumListing, AlbumSummary, Artist,
    ArtistListing, ArtistSummary, MusicTrack, PLAYLIST_GET, PLAYLIST_LIST, Playlist,
    PlaylistListing, PlaylistSummary,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A pin of one album.
pub const PIN_ALBUM: &str = "album";
/// A pin of one artist: their albums, and the tracks they play on elsewhere.
pub const PIN_ARTIST: &str = "artist";
/// A pin of one playlist.
pub const PIN_PLAYLIST: &str = "playlist";

/// Copy kind: a whole album (id = the album id).
const DOC_ALBUM: &str = "album";
/// Copy kind: the tracks of one artist on an album that is not theirs (id =
/// `"<album id>\n<artist id>"`), as an [`Album`] cut down to those tracks.
const DOC_ALBUM_PART: &str = "album-part";
const DOC_ARTIST: &str = "artist";
const DOC_PLAYLIST: &str = "playlist";

/// The preset of the audio a track is saved as.
const PRESET_AUDIO: &str = "offline-audio";
const PRESET_THUMB: &str = "thumb";
/// Bytes of offline audio per millisecond of track.
const AUDIO_BYTES_PER_MS: u64 = 12;
/// The length assumed for a track that does not say (4 minutes).
const DEFAULT_DURATION_MS: u64 = 240_000;
const THUMB_ESTIMATE: u64 = 20_000;
/// The JSON-RPC code of a missing target.
const RPC_NOT_FOUND: i32 = -32001;

/// The music app's [`OfflineCatalog`]: albums, artists and playlists.
pub struct MusicCatalog;

fn get<T: DeserializeOwned>(
    rpc: &mut dyn Fetch,
    method: &str,
    id: &str,
) -> Result<(T, serde_json::Value), ClientError> {
    let value = rpc.call(method, serde_json::json!({ "id": id }))?;
    let typed = serde_json::from_value(value.clone())
        .map_err(|e| ClientError::Protocol(format!("{method}: {e}")))?;
    Ok((typed, value))
}

fn is_not_found(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::NotFound
            | ClientError::Rpc {
                code: RPC_NOT_FOUND,
                ..
            }
    )
}

fn doc(kind: &str, id: String, value: &impl Serialize) -> Result<Doc, ClientError> {
    let value = serde_json::to_value(value).map_err(|e| ClientError::Protocol(e.to_string()))?;
    Ok(Doc {
        kind: kind.into(),
        id,
        value,
    })
}

/// `album` cut down to the tracks `artist` appears on.
fn part_of(mut album: Album, artist: &str) -> Album {
    album
        .tracks
        .retain(|t| t.tags.artists.iter().any(|a| a == artist));
    album
}

fn track_media(track: &MusicTrack, out: &mut Vec<MediaKey>) {
    let Some(hash) = &track.item.file_hash else {
        return;
    };
    let audio = track.tags.duration_ms.unwrap_or(DEFAULT_DURATION_MS) * AUDIO_BYTES_PER_MS;
    for (preset, estimate) in [(PRESET_AUDIO, audio), (PRESET_THUMB, THUMB_ESTIMATE)] {
        let key = MediaKey {
            item: track.item.id.clone(),
            file_hash: hash.clone(),
            preset: preset.into(),
            estimate,
        };
        if !out.contains(&key) {
            out.push(key);
        }
    }
}

impl OfflineCatalog for MusicCatalog {
    fn fetch(&self, pin: &PinTarget, rpc: &mut dyn Fetch) -> Result<Vec<Doc>, ClientError> {
        match pin.kind.as_str() {
            PIN_ALBUM => {
                let (album, _): (Album, _) = get(rpc, ALBUM_GET, &pin.id)?;
                Ok(vec![doc(DOC_ALBUM, pin.id.clone(), &album)?])
            }
            PIN_ARTIST => {
                let (artist, _): (Artist, _) = get(rpc, ARTIST_GET, &pin.id)?;
                let mut docs = Vec::new();
                for summary in &artist.albums {
                    // An album that vanished since the listing is left out; the artist stays.
                    match get::<Album>(rpc, ALBUM_GET, &summary.id.0) {
                        Ok((album, _)) => docs.push(doc(DOC_ALBUM, summary.id.0.clone(), &album)?),
                        Err(e) if is_not_found(&e) => {}
                        Err(e) => return Err(e),
                    }
                }
                for summary in &artist.appears_on {
                    match get::<Album>(rpc, ALBUM_GET, &summary.id.0) {
                        Ok((album, _)) => {
                            let id = format!("{}\n{}", summary.id.0, pin.id);
                            let part = part_of(album, &artist.summary.name);
                            docs.push(doc(DOC_ALBUM_PART, id, &part)?);
                        }
                        Err(e) if is_not_found(&e) => {}
                        Err(e) => return Err(e),
                    }
                }
                docs.push(doc(DOC_ARTIST, pin.id.clone(), &artist)?);
                Ok(docs)
            }
            PIN_PLAYLIST => {
                // A playlist cut short is kept as it came.
                let (playlist, _): (Playlist, _) = get(rpc, PLAYLIST_GET, &pin.id)?;
                Ok(vec![doc(DOC_PLAYLIST, pin.id.clone(), &playlist)?])
            }
            other => Err(ClientError::Protocol(format!("unknown pin kind {other}"))),
        }
    }

    fn media_of(&self, doc: &Doc) -> Vec<MediaKey> {
        let mut out = Vec::new();
        match doc.kind.as_str() {
            DOC_ALBUM | DOC_ALBUM_PART => {
                if let Ok(album) = serde_json::from_value::<Album>(doc.value.clone()) {
                    for track in &album.tracks {
                        track_media(track, &mut out);
                    }
                    // The cover is one of the tracks' own thumbs: the same key as above.
                    if let Some(cover) = &album.summary.cover_item
                        && let Some(track) = album.tracks.iter().find(|t| &t.item.id == cover)
                    {
                        track_media(track, &mut out);
                    }
                }
            }
            DOC_PLAYLIST => {
                if let Ok(playlist) = serde_json::from_value::<Playlist>(doc.value.clone()) {
                    for track in playlist.entries.iter().filter_map(|e| e.track.as_ref()) {
                        track_media(track, &mut out);
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn answer(&self, request: &Request, docs: &Docs) -> Option<Result<Response, ClientError>> {
        let Request::Call { method, params } = request else {
            return None;
        };
        let value = match method.as_str() {
            ALBUM_GET => album_get(params, docs)?,
            ARTIST_GET => docs.get(DOC_ARTIST, params["id"].as_str()?)?.value.clone(),
            PLAYLIST_GET => docs
                .get(DOC_PLAYLIST, params["id"].as_str()?)?
                .value
                .clone(),
            ALBUM_LIST => album_list(params, docs),
            ARTIST_LIST => artist_list(params, docs),
            PLAYLIST_LIST => playlist_list(params, docs),
            _ => return None,
        };
        Some(Ok(Response::Value(value)))
    }
}

/// The whole album if it is saved, else the first saved part of it.
fn album_get(params: &serde_json::Value, docs: &Docs) -> Option<serde_json::Value> {
    let id = params["id"].as_str()?;
    if let Some(doc) = docs.get(DOC_ALBUM, id) {
        return Some(doc.value.clone());
    }
    docs.of_kind(DOC_ALBUM_PART)
        .find(|d| d.value["summary"]["id"].as_str() == Some(id))
        .map(|d| d.value.clone())
}

/// The `summary` of every doc of `kind`.
fn summaries<'a, T: DeserializeOwned + 'a>(
    docs: &'a Docs,
    kind: &'a str,
) -> impl Iterator<Item = T> + 'a {
    docs.of_kind(kind)
        .filter_map(|d| serde_json::from_value(d.value.get("summary")?.clone()).ok())
}

/// Offline lists come in one page: a `cursor` asks for one that does not exist.
fn has_cursor(params: &serde_json::Value) -> bool {
    !params["cursor"].is_null()
}

fn album_list(params: &serde_json::Value, docs: &Docs) -> serde_json::Value {
    let mut albums: Vec<AlbumSummary> = Vec::new();
    if !has_cursor(params) {
        for s in summaries::<AlbumSummary>(docs, DOC_ALBUM).chain(summaries(docs, DOC_ALBUM_PART)) {
            if !albums.iter().any(|a| a.id == s.id) {
                albums.push(s);
            }
        }
    }
    albums
        .sort_by(|a, b| (a.title.to_lowercase(), &a.id.0).cmp(&(b.title.to_lowercase(), &b.id.0)));
    serde_json::to_value(AlbumListing {
        albums,
        next_cursor: None,
    })
    .unwrap_or_default()
}

fn artist_list(params: &serde_json::Value, docs: &Docs) -> serde_json::Value {
    let mut artists: Vec<ArtistSummary> = if has_cursor(params) {
        Vec::new()
    } else {
        summaries(docs, DOC_ARTIST).collect()
    };
    artists.sort_by_key(|a| a.name.to_lowercase());
    serde_json::to_value(ArtistListing {
        artists,
        next_cursor: None,
    })
    .unwrap_or_default()
}

fn playlist_list(params: &serde_json::Value, docs: &Docs) -> serde_json::Value {
    let mut playlists: Vec<PlaylistSummary> = if has_cursor(params) {
        Vec::new()
    } else {
        summaries(docs, DOC_PLAYLIST).collect()
    };
    playlists.sort_by_key(|p| p.name.to_lowercase());
    serde_json::to_value(PlaylistListing {
        playlists,
        next_cursor: None,
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asteria_music_api::{
        AlbumId, ArtistId, EntryStatus, ItemId, MediaItem, MediaKind, PlaylistEntry, PlaylistId,
        TrackTags,
    };
    use serde_json::{Value, json};

    /// A `Fetch` that answers from recorded JSON, keyed by method and id.
    struct Recorded(Vec<(&'static str, String, Result<Value, ClientError>)>);

    impl Fetch for Recorded {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
            let id = params["id"].as_str().unwrap_or_default();
            self.0
                .iter()
                .find(|(m, i, _)| *m == method && i == id)
                .map(|(_, _, r)| r.clone())
                .unwrap_or(Err(ClientError::NotFound))
        }
    }

    fn track(id: &str, hash: Option<&str>, artists: &[&str], duration: Option<u64>) -> MusicTrack {
        MusicTrack {
            item: MediaItem {
                id: ItemId(id.into()),
                path: format!("{id}.flac"),
                kind: MediaKind::Audio,
                size: 1,
                modified: "2026-01-01T00:00:00Z".into(),
                rating: None,
                subjects: vec![],
                hierarchical_subjects: vec![],
                description: None,
                label: None,
                content_hash: None,
                file_hash: hash.map(Into::into),
                read_only_reason: None,
            },
            tags: TrackTags {
                artists: artists.iter().map(|a| a.to_string()).collect(),
                duration_ms: duration,
                ..TrackTags::default()
            },
            display_title: id.into(),
        }
    }

    fn summary(id: &str, title: &str, cover: Option<&str>) -> AlbumSummary {
        AlbumSummary {
            id: AlbumId(id.into()),
            title: title.into(),
            album_artist: "Someone".into(),
            album_artist_id: ArtistId("ar".into()),
            year: None,
            track_count: 0,
            cover_item: cover.map(|c| ItemId(c.into())),
        }
    }

    fn album(id: &str, title: &str, cover: Option<&str>, tracks: Vec<MusicTrack>) -> Album {
        Album {
            summary: summary(id, title, cover),
            folder: id.into(),
            tracks,
        }
    }

    fn docs_of(docs: Vec<Doc>) -> Docs {
        Docs::new(docs, &MusicCatalog)
    }

    fn call(method: &str, params: Value) -> Request {
        Request::Call {
            method: method.into(),
            params,
        }
    }

    fn answer(docs: &Docs, method: &str, params: Value) -> Option<Value> {
        match MusicCatalog.answer(&call(method, params), docs) {
            Some(Ok(Response::Value(v))) => Some(v),
            Some(other) => panic!("unexpected {other:?}"),
            None => None,
        }
    }

    #[test]
    fn an_artist_pin_fetches_its_albums_and_the_tracks_it_plays_on() {
        let artist = Artist {
            summary: ArtistSummary {
                id: ArtistId("ar".into()),
                name: "Ann".into(),
                album_count: 1,
            },
            albums: vec![summary("own", "Own", None)],
            appears_on: vec![summary("guest", "Guest", None)],
        };
        let own = album(
            "own",
            "Own",
            None,
            vec![track("t1", Some("h1"), &["Ann"], None)],
        );
        let guest = album(
            "guest",
            "Guest",
            None,
            vec![
                track("g1", Some("h2"), &["Bob"], None),
                track("g2", Some("h3"), &["Bob", "Ann"], None),
                track("g3", Some("h4"), &["Cy"], None),
            ],
        );
        let mut rpc = Recorded(vec![
            (
                ARTIST_GET,
                "ar".into(),
                Ok(serde_json::to_value(&artist).unwrap()),
            ),
            (
                ALBUM_GET,
                "own".into(),
                Ok(serde_json::to_value(&own).unwrap()),
            ),
            (
                ALBUM_GET,
                "guest".into(),
                Ok(serde_json::to_value(&guest).unwrap()),
            ),
        ]);
        let pin = PinTarget {
            kind: PIN_ARTIST.into(),
            id: "ar".into(),
            name: "Ann".into(),
        };
        let docs = MusicCatalog.fetch(&pin, &mut rpc).unwrap();
        let of = |k: &str| docs.iter().filter(|d| d.kind == k).collect::<Vec<_>>();
        assert_eq!(of("artist").len(), 1);
        assert_eq!(of("album").len(), 1);
        let parts = of("album-part");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].id, "guest\nar");
        let part: Album = serde_json::from_value(parts[0].value.clone()).unwrap();
        assert_eq!(part.tracks.len(), 1);
        assert_eq!(part.tracks[0].item.id.0, "g2");
    }

    #[test]
    fn an_album_that_vanished_is_skipped_but_other_errors_fail_the_pin() {
        let artist = Artist {
            summary: ArtistSummary {
                id: ArtistId("ar".into()),
                name: "Ann".into(),
                album_count: 1,
            },
            albums: vec![summary("gone", "Gone", None)],
            appears_on: vec![],
        };
        let pin = PinTarget {
            kind: PIN_ARTIST.into(),
            id: "ar".into(),
            name: "Ann".into(),
        };
        let artist = serde_json::to_value(&artist).unwrap();
        let mut rpc = Recorded(vec![
            (ARTIST_GET, "ar".into(), Ok(artist.clone())),
            (
                ALBUM_GET,
                "gone".into(),
                Err(ClientError::Rpc {
                    code: -32001,
                    message: "no".into(),
                }),
            ),
        ]);
        let docs = MusicCatalog.fetch(&pin, &mut rpc).unwrap();
        assert_eq!(docs.len(), 1);
        let mut rpc = Recorded(vec![
            (ARTIST_GET, "ar".into(), Ok(artist)),
            (ALBUM_GET, "gone".into(), Err(ClientError::Unauthorized)),
        ]);
        assert!(matches!(
            MusicCatalog.fetch(&pin, &mut rpc),
            Err(ClientError::Unauthorized)
        ));
    }

    #[test]
    fn media_of_an_album_is_audio_and_thumbs() {
        let a = album(
            "a",
            "A",
            Some("t1"),
            vec![
                track("t1", Some("h1"), &[], Some(1000)),
                track("t2", Some("h2"), &[], None),
                track("t3", None, &[], Some(1000)),
            ],
        );
        let doc = Doc {
            kind: "album".into(),
            id: "a".into(),
            value: serde_json::to_value(&a).unwrap(),
        };
        let keys = MusicCatalog.media_of(&doc);
        let audio: Vec<_> = keys
            .iter()
            .filter(|k| k.preset == "offline-audio")
            .collect();
        let thumbs: Vec<_> = keys.iter().filter(|k| k.preset == "thumb").collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(thumbs.len(), 2);
        assert_eq!(audio[0].estimate, 12_000);
        assert_eq!(audio[1].estimate, 240_000 * 12);
        assert!(thumbs.iter().all(|k| k.estimate == 20_000));
    }

    #[test]
    fn media_of_a_playlist_skips_entries_without_a_track() {
        let entry = |track| PlaylistEntry {
            raw_path: "x".into(),
            title_hint: None,
            status: EntryStatus::Found,
            track,
        };
        let playlist = Playlist {
            summary: PlaylistSummary {
                id: PlaylistId("p".into()),
                name: "P".into(),
                path: "p.m3u".into(),
            },
            entries: vec![entry(Some(track("t1", Some("h1"), &[], None))), entry(None)],
            truncated: true,
        };
        let doc = Doc {
            kind: "playlist".into(),
            id: "p".into(),
            value: serde_json::to_value(&playlist).unwrap(),
        };
        assert_eq!(MusicCatalog.media_of(&doc).len(), 2);
    }

    fn album_doc(kind: &str, id: &str, a: &Album) -> Doc {
        Doc {
            kind: kind.into(),
            id: id.into(),
            value: serde_json::to_value(a).unwrap(),
        }
    }

    #[test]
    fn offline_lists_are_built_from_the_copies() {
        let docs = docs_of(vec![
            album_doc("album", "1", &album("1", "b", None, vec![])),
            album_doc("album", "2", &album("2", "A", None, vec![])),
        ]);
        let v = answer(&docs, ALBUM_LIST, json!({"cursor": null, "limit": 50})).unwrap();
        let listing: AlbumListing = serde_json::from_value(v).unwrap();
        let titles: Vec<_> = listing.albums.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["A", "b"]);
        assert_eq!(listing.next_cursor, None);
        let v = answer(&docs, ALBUM_LIST, json!({"cursor": "c", "limit": 50})).unwrap();
        let listing: AlbumListing = serde_json::from_value(v).unwrap();
        assert!(listing.albums.is_empty());
    }

    #[test]
    fn album_get_prefers_the_whole_album() {
        let whole = album("x", "Whole", None, vec![track("t1", None, &[], None)]);
        let part = album("x", "Part", None, vec![]);
        let both = docs_of(vec![
            album_doc("album-part", "x\nar", &part),
            album_doc("album", "x", &whole),
        ]);
        let v = answer(&both, ALBUM_GET, json!({"id": "x"})).unwrap();
        assert_eq!(v["summary"]["title"], "Whole");
        let only_part = docs_of(vec![album_doc("album-part", "x\nar", &part)]);
        let v = answer(&only_part, ALBUM_GET, json!({"id": "x"})).unwrap();
        assert_eq!(v["summary"]["title"], "Part");
    }

    #[test]
    fn unsaved_requests_are_not_answered() {
        let docs = docs_of(vec![]);
        assert!(answer(&docs, ALBUM_GET, json!({"id": "x"})).is_none());
        assert!(answer(&docs, ARTIST_GET, json!({"id": "x"})).is_none());
        assert!(MusicCatalog.answer(&Request::LibraryInfo, &docs).is_none());
    }
}
