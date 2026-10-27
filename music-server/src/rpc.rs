//! The music RPC methods: albums, artists and playlists.
//!
//! A listing's cursor is the ID of the last element of the previous page, and the next page
//! starts right after that ID. When a rebuilt index no longer has the ID, the cursor is
//! rejected and the client starts over.
//!
//! Tracks are always built from the cache record at a path that the index or `resolve`
//! produced; a path written in a playlist is never opened.

use std::cell::RefCell;

use asteria_framework::MediaKind;
use asteria_framework::ids::LibraryId;
use asteria_framework_server::{ExtContext, ServerError, item_id_for, media_item, rpc_error};
use asteria_music_api::{
    Album, AlbumGetParams, AlbumListing, AlbumSummary, Artist, ArtistGetParams, ArtistListing,
    ArtistSummary, EntryStatus, ListParams, MusicTrack, Playlist, PlaylistEntry, PlaylistGetParams,
    PlaylistId, PlaylistListing, PlaylistSummary, RpcError, codes,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

use crate::ext::{MusicExtension, decode_tags, is_playlist};
use crate::index::{IndexedAlbum, IndexedArtist};
use crate::playlist::{self, Resolved};
use crate::{cover, tags};

// The same paging as `folder.list` (private constants of framework-server's rpc.rs).
const DEFAULT_LIMIT: u32 = 200;
const MAX_LIMIT: u32 = 1000;
/// The hash scope `#ASTERIA-HASH` values are looked up in.
const PAYLOAD_SCOPE: &str = "media-payload/1";

fn params<P: DeserializeOwned>(v: Value) -> Result<P, RpcError> {
    serde_json::from_value(v)
        .map_err(|e| rpc_error(codes::INVALID_PARAMS, format!("invalid params: {e}")))
}

fn to_value<R: Serialize>(r: R) -> Result<Value, RpcError> {
    serde_json::to_value(r).map_err(|_| rpc_error(codes::INTERNAL_ERROR, "internal error"))
}

/// Cache errors may name server paths, so none of their text reaches the client.
fn internal(_: ServerError) -> RpcError {
    rpc_error(codes::INTERNAL_ERROR, "internal error")
}

fn not_found() -> RpcError {
    rpc_error(codes::NOT_FOUND, "not found")
}

/// The page of `items` that `p` asks for, and the cursor of the page after it.
fn page<'a, T>(
    items: &'a [T],
    p: &ListParams,
    id: impl Fn(&T) -> &str,
) -> Result<(&'a [T], Option<String>), RpcError> {
    let start = match p.cursor.as_deref() {
        None => 0,
        Some(cursor) => {
            items
                .iter()
                .position(|item| id(item) == cursor)
                .ok_or_else(|| rpc_error(codes::INVALID_PARAMS, "cursor is no longer valid"))?
                + 1
        }
    };
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let end = items.len().min(start.saturating_add(limit));
    let next = (end < items.len()).then(|| id(&items[end - 1]).to_owned());
    Ok((&items[start..end], next))
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn album_summary(library: &LibraryId, album: &IndexedAlbum, extras: &[String]) -> AlbumSummary {
    // An embedded picture first; else the first track, when its folders hold a cover image.
    let cover = album.embedded_cover_item.as_deref().or_else(|| {
        album
            .tracks
            .first()
            .filter(|first| cover::folder_image(first, extras.iter().map(String::as_str)).is_some())
            .map(String::as_str)
    });
    AlbumSummary {
        id: album.id.clone(),
        title: album.title.clone(),
        album_artist: album.album_artist.clone(),
        album_artist_id: album.album_artist_id.clone(),
        year: album.year,
        track_count: count(album.tracks.len()),
        cover_item: cover.map(|rel| item_id_for(library, rel)),
    }
}

fn artist_summary(artist: &IndexedArtist) -> ArtistSummary {
    ArtistSummary {
        id: artist.id.clone(),
        name: artist.name.clone(),
        album_count: count(artist.albums.len()),
    }
}

/// The audio item at `rel` as a track, or `None` when the cache has no such item.
fn track(ctx: &ExtContext, rel: &str) -> Result<Option<MusicTrack>, RpcError> {
    let Some(rec) = ctx.cache.get_by_path(rel).map_err(internal)? else {
        return Ok(None);
    };
    if rec.kind != MediaKind::Audio {
        return Ok(None);
    }
    let bytes = ctx.cache.app_data(rel).map_err(internal)?;
    let tags = decode_tags(bytes.as_deref().unwrap_or_default());
    Ok(Some(MusicTrack {
        display_title: tags::display_title(&tags, rel),
        item: media_item(rec, ctx.user, ctx.owner),
        tags,
    }))
}

pub fn album_list(ext: &MusicExtension, ctx: &ExtContext, v: Value) -> Result<Value, RpcError> {
    let p: ListParams = params(v)?;
    let (_, index, extras) = ext.snapshot(ctx).map_err(internal)?;
    let (albums, next_cursor) = page(index.albums(), &p, |a| &a.id.0)?;
    let library = ctx.library.config().id;
    to_value(AlbumListing {
        albums: albums
            .iter()
            .map(|a| album_summary(&library, a, &extras))
            .collect(),
        next_cursor,
    })
}

pub fn album_get(ext: &MusicExtension, ctx: &ExtContext, v: Value) -> Result<Value, RpcError> {
    let p: AlbumGetParams = params(v)?;
    let (_, index, extras) = ext.snapshot(ctx).map_err(internal)?;
    let album = index.album(&p.id).ok_or_else(not_found)?;
    let mut tracks = Vec::with_capacity(album.tracks.len());
    for rel in &album.tracks {
        tracks.extend(track(ctx, rel)?);
    }
    to_value(Album {
        summary: album_summary(&ctx.library.config().id, album, &extras),
        folder: album.key_folder.clone(),
        tracks,
    })
}

pub fn artist_list(ext: &MusicExtension, ctx: &ExtContext, v: Value) -> Result<Value, RpcError> {
    let p: ListParams = params(v)?;
    let (_, index, _) = ext.snapshot(ctx).map_err(internal)?;
    let (artists, next_cursor) = page(index.artists(), &p, |a| &a.id.0)?;
    to_value(ArtistListing {
        artists: artists.iter().map(artist_summary).collect(),
        next_cursor,
    })
}

pub fn artist_get(ext: &MusicExtension, ctx: &ExtContext, v: Value) -> Result<Value, RpcError> {
    let p: ArtistGetParams = params(v)?;
    let (_, index, extras) = ext.snapshot(ctx).map_err(internal)?;
    let artist = index.artist(&p.id).ok_or_else(not_found)?;
    let library = ctx.library.config().id;
    let summaries = |ids: &[_]| -> Vec<AlbumSummary> {
        ids.iter()
            .filter_map(|id| index.album(id))
            .map(|a| album_summary(&library, a, &extras))
            .collect()
    };
    to_value(Artist {
        summary: artist_summary(artist),
        albums: summaries(&artist.albums),
        appears_on: summaries(&artist.appears_on),
    })
}

fn playlist_id(library: &LibraryId, rel: &str) -> PlaylistId {
    let name = format!("playlist\n{rel}");
    PlaylistId(Uuid::new_v5(&library.0, name.as_bytes()).to_string())
}

/// The playlists among the extras, in path order.
fn playlists(library: &LibraryId, extras: &[String]) -> Vec<PlaylistSummary> {
    let mut rels: Vec<&String> = extras.iter().filter(|rel| is_playlist(rel)).collect();
    rels.sort();
    rels.into_iter()
        .map(|rel| {
            let file = rel.rsplit('/').next().unwrap_or(rel);
            let name = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
            PlaylistSummary {
                id: playlist_id(library, rel),
                name: name.to_owned(),
                path: rel.clone(),
            }
        })
        .collect()
}

pub fn playlist_list(ext: &MusicExtension, ctx: &ExtContext, v: Value) -> Result<Value, RpcError> {
    let p: ListParams = params(v)?;
    let (_, _, extras) = ext.snapshot(ctx).map_err(internal)?;
    let all = playlists(&ctx.library.config().id, &extras);
    let (page, next_cursor) = page(&all, &p, |pl| &pl.id.0)?;
    to_value(PlaylistListing {
        playlists: page.to_vec(),
        next_cursor,
    })
}

pub fn playlist_get(ext: &MusicExtension, ctx: &ExtContext, v: Value) -> Result<Value, RpcError> {
    let p: PlaylistGetParams = params(v)?;
    let (_, _, extras) = ext.snapshot(ctx).map_err(internal)?;
    let summary = playlists(&ctx.library.config().id, &extras)
        .into_iter()
        .find(|pl| pl.id == p.id)
        .ok_or_else(not_found)?;
    let root = ctx.library.root();
    // Only a file that stays inside the library, outside hidden folders, is read.
    let bytes = cover::canonical_in_library(root, &summary.path)
        .and_then(|canon| std::fs::read(canon).ok())
        .ok_or_else(not_found)?;
    let parsed = playlist::parse(&bytes);

    // `resolve` takes infallible lookups; the first cache error is kept and reported.
    let failure: RefCell<Option<ServerError>> = RefCell::new(None);
    let keep = |e| {
        failure.borrow_mut().get_or_insert(e);
    };
    let is_audio = |rel: &str| match ctx.cache.get_by_path(rel) {
        Ok(rec) => rec.is_some_and(|r| r.kind == MediaKind::Audio),
        Err(e) => {
            keep(e);
            false
        }
    };
    let by_hash = |hash: &str| match ctx.cache.paths_with_hash(hash, PAYLOAD_SCOPE) {
        Ok(paths) => paths.into_iter().filter(|p| is_audio(p)).collect(),
        Err(e) => {
            keep(e);
            Vec::new()
        }
    };

    let mut entries = Vec::with_capacity(parsed.entries.len());
    for entry in parsed.entries {
        let resolved = playlist::resolve(&summary.path, &entry, root, is_audio, by_hash);
        if let Some(e) = failure.take() {
            return Err(internal(e));
        }
        let (status, found) = match resolved {
            Resolved::Found(rel) => (EntryStatus::Found, track(ctx, &rel)?),
            Resolved::Moved(rel) => (EntryStatus::Moved, track(ctx, &rel)?),
            Resolved::Missing => (EntryStatus::Missing, None),
        };
        entries.push(PlaylistEntry {
            raw_path: entry.path,
            title_hint: entry.title_hint,
            // The item may have gone since it was resolved.
            status: if found.is_some() {
                status
            } else {
                EntryStatus::Missing
            },
            track: found,
        });
    }
    to_value(Playlist {
        summary,
        entries,
        truncated: parsed.truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use asteria_framework::journal::Actor;
    use asteria_framework::{Library, MediaKind};
    use asteria_framework_server::{
        AppExtension, Cache, ExtContext, ExtraRecord, ThumbSource, hash_pending, item_id_for, scan,
    };
    use asteria_music_api::*;
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::*;
    use lofty::tag::{ItemKey, ItemValue, Tag, TagItem};
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use crate::ext::MusicExtension;

    struct Env {
        _dir: tempfile::TempDir,
        root: PathBuf,
        lib: Arc<Library>,
        cache: Arc<Cache>,
        ext: MusicExtension,
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/asteria-framework/tests/fixtures")
            .join(name)
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state"));
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
        Env {
            _dir: dir,
            root,
            lib: Arc::new(lib),
            cache: Arc::new(cache),
            ext: MusicExtension::new(),
        }
    }

    impl Env {
        fn scan(&self) {
            scan(&self.lib, &self.cache, &self.ext).unwrap();
        }

        fn ctx(&self) -> ExtContext {
            ExtContext {
                library: self.lib.clone(),
                cache: self.cache.clone(),
                user: self.lib.owner(),
                owner: self.lib.owner(),
            }
        }

        fn call(&self, method: &str, params: Value) -> Option<Result<Value, RpcError>> {
            self.ext.rpc(&self.ctx(), method, params)
        }

        fn ok<T: DeserializeOwned>(&self, method: &str, params: Value) -> T {
            let v = self.call(method, params).expect("handled").expect("ok");
            serde_json::from_value(v).unwrap()
        }

        fn err(&self, method: &str, params: Value) -> RpcError {
            self.call(method, params)
                .expect("handled")
                .expect_err("an error")
        }

        /// Copies `fixture` to `rel` and writes the given tag items (with `pics` embedded).
        fn track_with(
            &self,
            rel: &str,
            fixture_name: &str,
            items: &[(ItemKey, &str)],
            pics: &[&[u8]],
        ) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::copy(fixture(fixture_name), &path).unwrap();
            let mut file = lofty::read_from_path(&path).unwrap();
            let mut tag = Tag::new(file.primary_tag_type());
            for (key, value) in items {
                tag.push(TagItem::new(key.clone(), ItemValue::Text((*value).into())));
            }
            for data in pics {
                tag.push_picture(Picture::new_unchecked(
                    PictureType::CoverFront,
                    Some(MimeType::Png),
                    None,
                    data.to_vec(),
                ));
            }
            file.insert_tag(tag);
            file.save_to_path(&path, WriteOptions::default()).unwrap();
        }

        fn track(&self, rel: &str, fixture_name: &str, items: &[(ItemKey, &str)]) {
            self.track_with(rel, fixture_name, items, &[]);
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn id(&self, rel: &str) -> ItemId {
            item_id_for(&self.lib.config().id, rel)
        }

        /// The current media-payload hash of the item at `rel`.
        fn payload_hash(&self, rel: &str) -> String {
            hash_pending(&self.lib, &self.cache, &AtomicBool::new(false)).unwrap();
            let h = self.cache.get_by_path(rel).unwrap().unwrap().hash.unwrap();
            assert_eq!(h.scope, "media-payload/1");
            h.hash
        }

        /// Three albums: "Album One" by A (track 2 features B), "B Album" by B and a
        /// compilation "Comp" by Various Artists.
        fn library(&self) {
            use ItemKey::*;
            self.track(
                "Artist A/Album One/02.flac",
                "sine440.flac",
                &[
                    (TrackTitle, "Second"),
                    (TrackArtist, "A"),
                    (TrackArtist, "B"),
                    (AlbumTitle, "Album One"),
                    (AlbumArtist, "A"),
                    (TrackNumber, "2"),
                ],
            );
            self.track(
                "Artist A/Album One/01.flac",
                "sine440.flac",
                &[
                    (TrackTitle, "First"),
                    (TrackArtist, "A"),
                    (AlbumTitle, "Album One"),
                    (AlbumArtist, "A"),
                    (TrackNumber, "1"),
                    (RecordingDate, "2001"),
                ],
            );
            self.track(
                "B/B Album/01.flac",
                "sine880.flac",
                &[
                    (TrackTitle, "Bee"),
                    (TrackArtist, "B"),
                    (AlbumTitle, "B Album"),
                    (AlbumArtist, "B"),
                ],
            );
            self.track(
                "Various/Comp/01.flac",
                "sine440.flac",
                &[(TrackTitle, "C1"), (TrackArtist, "C"), (AlbumTitle, "Comp")],
            );
            self.track(
                "Various/Comp/02.flac",
                "sine440.flac",
                &[(TrackTitle, "D1"), (TrackArtist, "D"), (AlbumTitle, "Comp")],
            );
        }

        fn albums(&self) -> Vec<AlbumSummary> {
            let listing: AlbumListing = self.ok(ALBUM_LIST, json!({}));
            assert_eq!(listing.next_cursor, None);
            listing.albums
        }

        fn album_named(&self, title: &str) -> AlbumSummary {
            self.albums()
                .into_iter()
                .find(|a| a.title == title)
                .unwrap_or_else(|| panic!("no album {title}"))
        }

        fn artist_named(&self, name: &str) -> ArtistSummary {
            let listing: ArtistListing = self.ok(ARTIST_LIST, json!({}));
            listing
                .artists
                .into_iter()
                .find(|a| a.name == name)
                .unwrap_or_else(|| panic!("no artist {name}"))
        }
    }

    fn titles(albums: &[AlbumSummary]) -> Vec<&str> {
        albums.iter().map(|a| a.title.as_str()).collect()
    }

    #[test]
    fn album_list_pages_with_cursor() {
        let e = env();
        e.library();
        e.scan();

        let first: AlbumListing = e.ok(ALBUM_LIST, json!({ "limit": 2 }));
        assert_eq!(titles(&first.albums), ["Album One", "B Album"]);
        let cursor = first.next_cursor.expect("a next page");
        assert_eq!(cursor, first.albums[1].id.0, "the cursor is the last ID");

        let second: AlbumListing = e.ok(ALBUM_LIST, json!({ "cursor": cursor, "limit": 2 }));
        assert_eq!(titles(&second.albums), ["Comp"]);
        assert_eq!(second.next_cursor, None);

        let one = &first.albums[0];
        assert_eq!(one.album_artist, "A");
        assert_eq!(one.year, Some(2001));
        assert_eq!(one.track_count, 2);
        assert_eq!(second.albums[0].album_artist, "Various Artists");
    }

    #[test]
    fn stale_cursor_is_invalid_params() {
        let e = env();
        e.library();
        e.scan();
        let first: AlbumListing = e.ok(ALBUM_LIST, json!({ "limit": 1 }));
        let cursor = first.next_cursor.unwrap();

        // The album the cursor names goes away.
        fs::remove_dir_all(e.root.join("Artist A")).unwrap();
        e.scan();
        let err = e.err(ALBUM_LIST, json!({ "cursor": cursor }));
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert_eq!(err.message, "cursor is no longer valid");
    }

    #[test]
    fn album_get_returns_ordered_tracks_with_ratings() {
        let e = env();
        e.library();
        e.scan();
        let owner = e.lib.owner();
        e.lib
            .set_rating(
                Path::new("Artist A/Album One/02.flac"),
                owner,
                4,
                chrono::Utc::now(),
                Actor::User(owner),
            )
            .unwrap();
        e.scan();

        let summary = e.album_named("Album One");
        let album: Album = e.ok(ALBUM_GET, json!({ "id": summary.id }));
        assert_eq!(album.summary, summary);
        assert_eq!(album.folder, "Artist A/Album One");
        let got: Vec<(&str, &str, Option<i8>)> = album
            .tracks
            .iter()
            .map(|t| {
                (
                    t.item.path.as_str(),
                    t.display_title.as_str(),
                    t.item.rating,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("Artist A/Album One/01.flac", "First", None),
                ("Artist A/Album One/02.flac", "Second", Some(4)),
            ]
        );
        assert_eq!(album.tracks[1].tags.artists, ["A", "B"]);
        assert_eq!(album.tracks[1].tags.track, Some(2));
        assert_eq!(album.tracks[0].item.kind, MediaKind::Audio);
    }

    #[test]
    fn retag_and_rescan_updates_the_index() {
        let e = env();
        e.library();
        e.scan();
        assert!(titles(&e.albums()).contains(&"B Album"));

        e.track(
            "B/B Album/01.flac",
            "sine880.flac",
            &[
                (ItemKey::TrackTitle, "Bee"),
                (ItemKey::TrackArtist, "B"),
                (ItemKey::AlbumTitle, "Renamed Record"),
                (ItemKey::AlbumArtist, "B"),
            ],
        );
        e.scan();
        let albums = e.albums();
        assert!(titles(&albums).contains(&"Renamed Record"));
        assert!(!titles(&albums).contains(&"B Album"));
    }

    #[test]
    fn deleted_tracks_drop_empty_albums() {
        let e = env();
        e.library();
        e.scan();
        let comp = e.album_named("Comp");

        fs::remove_file(e.root.join("Various/Comp/01.flac")).unwrap();
        e.scan();
        assert_eq!(e.album_named("Comp").track_count, 1);

        fs::remove_file(e.root.join("Various/Comp/02.flac")).unwrap();
        e.scan();
        assert_eq!(titles(&e.albums()), ["Album One", "B Album"]);
        let err = e.err(ALBUM_GET, json!({ "id": comp.id }));
        assert_eq!(err.code, codes::NOT_FOUND);
    }

    #[test]
    fn artist_get_lists_albums_and_appears_on() {
        let e = env();
        e.library();
        e.scan();

        let listing: ArtistListing = e.ok(ARTIST_LIST, json!({}));
        let names: Vec<&str> = listing.artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["A", "B", "Various Artists"]);
        assert_eq!(listing.next_cursor, None);

        let paged: ArtistListing = e.ok(ARTIST_LIST, json!({ "limit": 1 }));
        assert_eq!(paged.artists[0].name, "A");
        let cursor = paged.next_cursor.unwrap();
        let rest: ArtistListing = e.ok(ARTIST_LIST, json!({ "cursor": cursor, "limit": 5 }));
        assert_eq!(rest.artists.len(), 2);
        assert_eq!(rest.next_cursor, None);

        let b = e.artist_named("B");
        assert_eq!(b.album_count, 1, "appears-on albums do not count");
        let artist: Artist = e.ok(ARTIST_GET, json!({ "id": b.id }));
        assert_eq!(artist.summary, b);
        assert_eq!(titles(&artist.albums), ["B Album"]);
        assert_eq!(titles(&artist.appears_on), ["Album One"]);
        assert_eq!(artist.albums[0], e.album_named("B Album"));
    }

    #[test]
    fn album_cover_item_prefers_embedded_then_folder_image() {
        let e = env();
        e.library();
        // Album One: only its second track has an embedded picture.
        e.track_with(
            "Artist A/Album One/02.flac",
            "sine440.flac",
            &[
                (ItemKey::TrackArtist, "A"),
                (ItemKey::AlbumTitle, "Album One"),
                (ItemKey::AlbumArtist, "A"),
                (ItemKey::TrackNumber, "2"),
            ],
            &[b"png-bytes"],
        );
        // B Album: a folder image.
        fs::copy(fixture("red.jpg"), e.root.join("B/B Album/Cover.JPG")).unwrap();
        e.scan();

        assert_eq!(
            e.album_named("Album One").cover_item,
            Some(e.id("Artist A/Album One/02.flac"))
        );
        assert_eq!(
            e.album_named("B Album").cover_item,
            Some(e.id("B/B Album/01.flac"))
        );
        assert_eq!(e.album_named("Comp").cover_item, None);

        // The folder image is the track's thumbnail.
        let rec = e.cache.get_by_path("B/B Album/01.flac").unwrap().unwrap();
        let abs = e.root.join(&rec.path);
        match e.ext.thumb_source(&e.ctx(), &rec, &abs) {
            ThumbSource::Image { bytes, ext, .. } => {
                assert_eq!(bytes, fs::read(fixture("red.jpg")).unwrap());
                assert_eq!(ext, "jpg");
            }
            other => panic!("expected an image, got {other:?}"),
        }
        let rec = e
            .cache
            .get_by_path("Various/Comp/01.flac")
            .unwrap()
            .unwrap();
        let abs = e.root.join(&rec.path);
        assert_eq!(e.ext.thumb_source(&e.ctx(), &rec, &abs), ThumbSource::None);
    }

    #[test]
    fn wants_playlists_and_cover_images_only() {
        let ext = MusicExtension::new();
        assert!(ext.wants_extra("a/mix.m3u8"));
        assert!(ext.wants_extra("a/OLD.M3U"));
        assert!(ext.wants_extra("a/b/Folder.PNG"));
        assert!(!ext.wants_extra("a/back.jpg"));
        assert!(!ext.wants_extra("a/notes.txt"));
        assert!(!ext.wants_extra("m3u8"));
        assert!(ext.accepts(MediaKind::Audio));
        assert!(!ext.accepts(MediaKind::Image));
    }

    #[test]
    fn playlist_list_and_get() {
        let e = env();
        e.library();
        e.scan();
        let hash = e.payload_hash("B/B Album/01.flac");
        let mix = format!(
            "#EXTM3U\n\
             #EXTINF:1,First\n\
             ../Artist A/Album One/01.flac\n\
             #ASTERIA-HASH:{hash}\n\
             #EXTINF:1,Bee\n\
             ../B/B Album/01.flac\n\
             #EXTINF:1,Gone\n\
             ../nowhere.flac\n"
        );
        e.write("Lists/mix.m3u8", mix.as_bytes());
        e.write("Lists/old.M3U", b"..\\Various\\Comp\\02.flac\r\n");
        e.write("Lists/notes.txt", b"not a playlist");
        // The track named by the hash moves.
        fs::rename(
            e.root.join("B/B Album/01.flac"),
            e.root.join("B/B Album/moved.flac"),
        )
        .unwrap();
        e.scan();
        hash_pending(&e.lib, &e.cache, &AtomicBool::new(false)).unwrap();

        let listing: PlaylistListing = e.ok(PLAYLIST_LIST, json!({}));
        let got: Vec<(&str, &str)> = listing
            .playlists
            .iter()
            .map(|p| (p.name.as_str(), p.path.as_str()))
            .collect();
        assert_eq!(got, [("mix", "Lists/mix.m3u8"), ("old", "Lists/old.M3U")]);
        assert_eq!(listing.next_cursor, None);
        let paged: PlaylistListing = e.ok(PLAYLIST_LIST, json!({ "limit": 1 }));
        assert_eq!(
            paged.next_cursor.as_deref(),
            Some(listing.playlists[0].id.0.as_str())
        );

        let mix_summary = listing.playlists[0].clone();
        let pl: Playlist = e.ok(PLAYLIST_GET, json!({ "id": mix_summary.id }));
        assert_eq!(pl.summary, mix_summary);
        assert!(!pl.truncated);
        let got: Vec<(&str, Option<&str>, EntryStatus, Option<&str>)> = pl
            .entries
            .iter()
            .map(|en| {
                (
                    en.raw_path.as_str(),
                    en.title_hint.as_deref(),
                    en.status,
                    en.track.as_ref().map(|t| t.item.path.as_str()),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "../Artist A/Album One/01.flac",
                    Some("First"),
                    EntryStatus::Found,
                    Some("Artist A/Album One/01.flac"),
                ),
                (
                    "../B/B Album/01.flac",
                    Some("Bee"),
                    EntryStatus::Moved,
                    Some("B/B Album/moved.flac"),
                ),
                ("../nowhere.flac", Some("Gone"), EntryStatus::Missing, None),
            ]
        );
        assert_eq!(pl.entries[0].track.as_ref().unwrap().display_title, "First");

        let old: Playlist = e.ok(PLAYLIST_GET, json!({ "id": listing.playlists[1].id }));
        assert_eq!(old.entries[0].status, EntryStatus::Found);
        assert_eq!(
            old.entries[0].track.as_ref().unwrap().item.path,
            "Various/Comp/02.flac"
        );
    }

    #[test]
    fn playlist_entries_naming_non_tracks_are_missing() {
        let e = env();
        e.library();
        e.write("Lists/cover.jpg", b"x");
        e.write(
            "Lists/odd.m3u8",
            b"cover.jpg\nodd.m3u8\n../../outside.flac\n../.asteria/library.toml\n",
        );
        e.scan();
        let listing: PlaylistListing = e.ok(PLAYLIST_LIST, json!({}));
        let pl: Playlist = e.ok(PLAYLIST_GET, json!({ "id": listing.playlists[0].id }));
        assert_eq!(pl.entries.len(), 4);
        assert!(
            pl.entries
                .iter()
                .all(|en| en.status == EntryStatus::Missing)
        );
        assert!(pl.entries.iter().all(|en| en.track.is_none()));
    }

    #[cfg(unix)]
    #[test]
    fn playlist_outside_the_library_is_not_found() {
        let e = env();
        e.library();
        e.scan();
        // A playlist recorded in EXTRAS whose file is a symlink out of the library.
        let outside = e._dir.path().join("outside.m3u8");
        fs::write(&outside, b"../Artist A/Album One/01.flac\n").unwrap();
        std::os::unix::fs::symlink(&outside, e.root.join("evil.m3u8")).unwrap();
        e.cache
            .put_extras(&[(
                "evil.m3u8".to_owned(),
                ExtraRecord {
                    size: 1,
                    mtime_ns: 0,
                },
            )])
            .unwrap();
        let listing: PlaylistListing = e.ok(PLAYLIST_LIST, json!({}));
        assert_eq!(listing.playlists.len(), 1);
        let err = e.err(PLAYLIST_GET, json!({ "id": listing.playlists[0].id }));
        assert_eq!(err.code, codes::NOT_FOUND);
        assert!(!err.message.contains('/'), "no paths: {}", err.message);
    }

    #[test]
    fn unknown_ids_are_not_found() {
        let e = env();
        e.library();
        e.scan();
        let nil = "00000000-0000-0000-0000-000000000000";
        for method in [ALBUM_GET, ARTIST_GET, PLAYLIST_GET] {
            let err = e.err(method, json!({ "id": nil }));
            assert_eq!(err.code, codes::NOT_FOUND, "{method}");
        }
        for method in [ALBUM_GET, ARTIST_GET, PLAYLIST_GET] {
            let err = e.err(method, json!({}));
            assert_eq!(err.code, codes::INVALID_PARAMS, "{method}");
        }
        for method in [ALBUM_LIST, ARTIST_LIST, PLAYLIST_LIST] {
            let err = e.err(method, json!({ "limit": "many" }));
            assert_eq!(err.code, codes::INVALID_PARAMS, "{method}");
            let err = e.err(method, json!({ "cursor": nil }));
            assert_eq!(err.code, codes::INVALID_PARAMS, "{method}");
        }
    }

    #[test]
    fn unknown_method_is_not_handled() {
        let e = env();
        assert!(e.call("album.nope", json!({})).is_none());
        assert!(e.call("folder.list", json!({})).is_none());
    }

    #[test]
    fn unreadable_cache_entries_still_list_as_tracks() {
        let e = env();
        e.library();
        e.scan();
        // A cache entry an older or broken server wrote.
        e.cache
            .put_app_data_many(&[(
                "Various/Comp/01.flac".to_owned(),
                Some(b"{not json".to_vec()),
            )])
            .unwrap();
        let comp = e.album_named("Comp");
        let album: Album = e.ok(ALBUM_GET, json!({ "id": comp.id }));
        let first = &album.tracks[0];
        assert_eq!(first.item.path, "Various/Comp/01.flac");
        assert_eq!(first.display_title, "01");
        assert_eq!(
            first.tags.tag_error.as_deref(),
            Some("unreadable cache entry")
        );
        assert_eq!(album.tracks.len(), 2);
    }
}
