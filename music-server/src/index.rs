//! The album and artist index, built in memory from the cached tags.
//!
//! The rules are those of the design spec §4.9: an album is a folder plus an album name,
//! the artist list is made of album artists, and names compare under NFKC and lower case.

#![cfg_attr(not(test), expect(dead_code, reason = "used by the music RPC methods"))]

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use asteria_framework::ids::LibraryId;
use asteria_music_api::{AlbumId, ArtistId, TrackTags};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

/// The album artist of a compilation, spelt this way on the wire.
const VARIOUS_ARTISTS: &str = "Various Artists";
/// The album title when a track has neither an album tag nor a folder to name it.
const UNKNOWN_ALBUM: &str = "Unknown Album";

#[derive(Debug, Clone, PartialEq)]
pub struct IndexedAlbum {
    pub id: AlbumId,
    pub key_folder: String,
    pub title: String,
    pub album_artist: String,
    pub album_artist_id: ArtistId,
    pub year: Option<i32>,
    /// Rel paths, ordered by disc, track and file name.
    pub tracks: Vec<String>,
    /// Rel path of the first track that has an embedded cover.
    pub embedded_cover_item: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexedArtist {
    pub id: ArtistId,
    pub name: String,
    /// Albums whose album artist is this artist.
    pub albums: Vec<AlbumId>,
    /// Other albums with a track by this artist.
    pub appears_on: Vec<AlbumId>,
}

#[derive(Debug, Default)]
pub struct Index {
    albums: Vec<IndexedAlbum>,
    artists: Vec<IndexedArtist>,
    album_by_id: HashMap<AlbumId, usize>,
    artist_by_id: HashMap<ArtistId, usize>,
}

/// What the build keeps of one track.
struct Track {
    rel: String,
    tags: TrackTags,
    folder: String,
    /// The disc from the tag, else from a disc folder's number.
    disc: Option<u32>,
}

/// An artist name seen in the tags, counted by spelling.
#[derive(Default)]
struct NameVotes {
    spellings: HashMap<String, usize>,
    sorts: HashMap<String, usize>,
}

impl NameVotes {
    fn vote(&mut self, spelling: &str, sort: Option<&str>) {
        *self.spellings.entry(spelling.to_owned()).or_default() += 1;
        if let Some(sort) = sort {
            *self.sorts.entry(sort.to_owned()).or_default() += 1;
        }
    }

    fn name(&self) -> String {
        most_common(&self.spellings).unwrap_or_default()
    }

    fn sort_name(&self) -> String {
        most_common(&self.sorts).unwrap_or_else(|| self.name())
    }
}

/// The most common string, the first in string order on a tie.
fn most_common(counts: &HashMap<String, usize>) -> Option<String> {
    counts
        .iter()
        .max_by(|(a, na), (b, nb)| na.cmp(nb).then_with(|| b.cmp(a)))
        .map(|(s, _)| s.clone())
}

/// A tag value with nothing but blanks is no value.
fn clean(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The digits of a disc folder name (`CD1`, `cd 2`, `Disc 10`, `disk3`), if it is one.
fn disc_digits(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let rest = ["cd", "disc", "disk"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))?;
    let digits = rest.trim_start();
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| digits.to_owned())
}

fn split_parent(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

/// The folder of the album a track belongs to: its own folder, or the parent of a disc folder.
pub fn album_folder(rel: &str) -> &str {
    let (dir, _) = split_parent(rel);
    let (parent, name) = split_parent(dir);
    if disc_digits(name).is_some() {
        parent
    } else {
        dir
    }
}

/// The key two names are compared by: NFKC, lower case, trimmed.
pub fn name_key(s: &str) -> String {
    s.nfkc()
        .collect::<String>()
        .to_lowercase()
        .trim()
        .to_owned()
}

fn album_id(library: &LibraryId, folder: &str, title_key: &str) -> AlbumId {
    let name = format!("album\n{folder}\n{title_key}");
    AlbumId(Uuid::new_v5(&library.0, name.as_bytes()).to_string())
}

fn artist_id(library: &LibraryId, key: &str) -> ArtistId {
    let name = format!("artist\n{key}");
    ArtistId(Uuid::new_v5(&library.0, name.as_bytes()).to_string())
}

/// An album before it is sorted.
struct Draft {
    album: IndexedAlbum,
    /// `name_key` of the album artist.
    artist_key: String,
    title_key: String,
    /// `name_key`s of every artist on a track.
    track_artists: BTreeSet<String>,
}

impl Index {
    pub fn build(
        library: &LibraryId,
        tracks: impl IntoIterator<Item = (String, TrackTags)>,
    ) -> Index {
        // Group by (album folder, album name).
        let mut groups: HashMap<(String, String), Vec<Track>> = HashMap::new();
        for (rel, tags) in tracks {
            let folder = album_folder(&rel).to_owned();
            let title = album_title(&tags, &folder);
            let disc = tags.disc.or_else(|| {
                let (dir, _) = split_parent(&rel);
                disc_digits(split_parent(dir).1)?.parse().ok()
            });
            groups
                .entry((folder.clone(), name_key(&title)))
                .or_default()
                .push(Track {
                    rel,
                    tags,
                    folder,
                    disc,
                });
        }

        let mut artist_votes: HashMap<String, NameVotes> = HashMap::new();
        let mut drafts = Vec::with_capacity(groups.len());
        for ((folder, title_key), mut group) in groups {
            group.sort_by(track_order);
            drafts.push(draft(library, folder, title_key, &group, &mut artist_votes));
        }

        // Artists exist for album artists only.
        let mut artists: Vec<(String, IndexedArtist, String)> = artist_votes
            .iter()
            .map(|(key, votes)| {
                let artist = IndexedArtist {
                    id: artist_id(library, key),
                    name: votes.name(),
                    albums: Vec::new(),
                    appears_on: Vec::new(),
                };
                (key.clone(), artist, name_key(&votes.sort_name()))
            })
            .collect();
        artists.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.id.0.cmp(&b.1.id.0)));
        let artist_pos: HashMap<&str, usize> = artists
            .iter()
            .enumerate()
            .map(|(i, (key, ..))| (key.as_str(), i))
            .collect();

        // Albums sort by the sort name of their artist, then year, title and ID.
        let sort_of = |d: &Draft| &artists[artist_pos[d.artist_key.as_str()]].2;
        drafts.sort_by(|a, b| {
            sort_of(a)
                .cmp(sort_of(b))
                .then_with(|| match (a.album.year, b.album.year) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                })
                .then_with(|| a.title_key.cmp(&b.title_key))
                .then_with(|| a.album.id.0.cmp(&b.album.id.0))
        });

        let mut out: Vec<IndexedArtist> = artists.iter().map(|(_, a, _)| a.clone()).collect();
        for d in &mut drafts {
            // An album shows the same spelling as the artist it points to.
            d.album.album_artist = out[artist_pos[d.artist_key.as_str()]].name.clone();
            out[artist_pos[d.artist_key.as_str()]]
                .albums
                .push(d.album.id.clone());
            for key in &d.track_artists {
                if *key == d.artist_key {
                    continue;
                }
                if let Some(&i) = artist_pos.get(key.as_str()) {
                    out[i].appears_on.push(d.album.id.clone());
                }
            }
        }

        let albums: Vec<IndexedAlbum> = drafts.into_iter().map(|d| d.album).collect();
        Index {
            album_by_id: albums
                .iter()
                .enumerate()
                .map(|(i, a)| (a.id.clone(), i))
                .collect(),
            artist_by_id: out
                .iter()
                .enumerate()
                .map(|(i, a)| (a.id.clone(), i))
                .collect(),
            albums,
            artists: out,
        }
    }

    pub fn albums(&self) -> &[IndexedAlbum] {
        &self.albums
    }

    pub fn album(&self, id: &AlbumId) -> Option<&IndexedAlbum> {
        self.album_by_id.get(id).map(|&i| &self.albums[i])
    }

    pub fn artists(&self) -> &[IndexedArtist] {
        &self.artists
    }

    pub fn artist(&self, id: &ArtistId) -> Option<&IndexedArtist> {
        self.artist_by_id.get(id).map(|&i| &self.artists[i])
    }
}

/// The album name tag, else the name of the album folder.
fn album_title(tags: &TrackTags, folder: &str) -> String {
    match clean(&tags.album) {
        Some(album) => album.to_owned(),
        None => match split_parent(folder).1 {
            "" => UNKNOWN_ALBUM.to_owned(),
            name => name.to_owned(),
        },
    }
}

/// Disc, then track (numbered before unnumbered), then file name.
fn track_order(a: &Track, b: &Track) -> Ordering {
    let num = |x: Option<u32>, y: Option<u32>| match (x, y) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    num(a.disc, b.disc)
        .then_with(|| num(a.tags.track, b.tags.track))
        .then_with(|| split_parent(&a.rel).1.cmp(split_parent(&b.rel).1))
        .then_with(|| a.rel.cmp(&b.rel))
}

fn draft(
    library: &LibraryId,
    folder: String,
    title_key: String,
    group: &[Track],
    artist_votes: &mut HashMap<String, NameVotes>,
) -> Draft {
    let mut titles: HashMap<String, usize> = HashMap::new();
    for t in group {
        *titles.entry(album_title(&t.tags, &t.folder)).or_default() += 1;
    }
    let title = most_common(&titles).unwrap_or_default();

    let mut track_artists = BTreeSet::new();
    for t in group {
        track_artists.extend(
            t.tags
                .artists
                .iter()
                .map(|a| a.trim())
                .filter(|a| !a.is_empty())
                .map(name_key),
        );
    }

    // The album artist: the tag's majority, else the one artist of every track, else VA.
    let mut tagged: HashMap<String, NameVotes> = HashMap::new();
    for t in group {
        if let Some(name) = clean(&t.tags.album_artist) {
            tagged
                .entry(name_key(name))
                .or_default()
                .vote(name, clean(&t.tags.album_artist_sort));
        }
    }
    let votes_of = |v: &NameVotes| v.spellings.values().sum::<usize>();
    let chosen = tagged
        .into_iter()
        .max_by(|(ka, va), (kb, vb)| votes_of(va).cmp(&votes_of(vb)).then_with(|| kb.cmp(ka)));
    let (artist_key, votes) = match chosen {
        Some(found) => found,
        None => single_artist(group).unwrap_or_else(|| {
            let mut v = NameVotes::default();
            v.vote(VARIOUS_ARTISTS, None);
            (name_key(VARIOUS_ARTISTS), v)
        }),
    };
    let album_artist = votes.name();
    let all = artist_votes.entry(artist_key.clone()).or_default();
    for (s, n) in &votes.spellings {
        *all.spellings.entry(s.clone()).or_default() += n;
    }
    for (s, n) in &votes.sorts {
        *all.sorts.entry(s.clone()).or_default() += n;
    }

    Draft {
        album: IndexedAlbum {
            id: album_id(library, &folder, &title_key),
            title,
            album_artist,
            album_artist_id: artist_id(library, &artist_key),
            year: group.iter().filter_map(|t| t.tags.year).min(),
            tracks: group.iter().map(|t| t.rel.clone()).collect(),
            embedded_cover_item: group
                .iter()
                .find(|t| t.tags.has_embedded_cover)
                .map(|t| t.rel.clone()),
            key_folder: folder,
        },
        artist_key,
        title_key,
        track_artists,
    }
}

/// The artist, when every track has the same non-empty artist list (the first one names it).
fn single_artist(group: &[Track]) -> Option<(String, NameVotes)> {
    let list = |t: &Track| -> Vec<String> {
        t.tags
            .artists
            .iter()
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .map(name_key)
            .collect()
    };
    let first = list(group.first()?);
    if first.is_empty() || group.iter().any(|t| list(t) != first) {
        return None;
    }
    let mut votes = NameVotes::default();
    for t in group {
        let name = t
            .tags
            .artists
            .iter()
            .map(|a| a.trim())
            .find(|a| !a.is_empty())?;
        votes.vote(name, clean(&t.tags.artist_sort));
    }
    Some((first[0].clone(), votes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> LibraryId {
        LibraryId(Uuid::from_u128(7))
    }

    fn t(artists: &[&str], album: Option<&str>) -> TrackTags {
        TrackTags {
            artists: artists.iter().map(|s| s.to_string()).collect(),
            album: album.map(str::to_owned),
            ..Default::default()
        }
    }

    fn build(items: Vec<(&str, TrackTags)>) -> Index {
        Index::build(&lib(), items.into_iter().map(|(r, t)| (r.to_owned(), t)))
    }

    #[test]
    fn album_artist_tag_wins_majority() {
        let tag = |aa: &str| TrackTags {
            album_artist: Some(aa.into()),
            ..t(&["x"], Some("Al"))
        };
        let idx = build(vec![
            ("d/1.flac", tag("Zed")),
            ("d/2.flac", tag("Amy")),
            ("d/3.flac", tag("Zed")),
        ]);
        assert_eq!(idx.albums()[0].album_artist, "Zed");
        // On a tie the name that sorts first wins.
        let idx = build(vec![("d/1.flac", tag("Zed")), ("d/2.flac", tag("Amy"))]);
        assert_eq!(idx.albums()[0].album_artist, "Amy");
    }

    #[test]
    fn single_artist_without_album_artist() {
        let idx = build(vec![
            ("d/1.flac", t(&["Amy"], Some("Al"))),
            ("d/2.flac", t(&["Amy"], Some("Al"))),
        ]);
        assert_eq!(idx.albums()[0].album_artist, "Amy");
        assert_eq!(idx.artists().len(), 1);
    }

    #[test]
    fn mixed_artists_become_various_artists() {
        let idx = build(vec![
            ("d/1.flac", t(&["Amy"], Some("Al"))),
            ("d/2.flac", t(&["Bob"], Some("Al"))),
        ]);
        let album = &idx.albums()[0];
        assert_eq!(album.album_artist, "Various Artists");
        assert_eq!(album.album_artist_id, artist_id(&lib(), "various artists"));
        // Tracks without any artist are not a single artist either.
        let idx = build(vec![("d/1.flac", t(&[], Some("Al")))]);
        assert_eq!(idx.albums()[0].album_artist, "Various Artists");
    }

    #[test]
    fn missing_album_tag_uses_folder_name() {
        let idx = build(vec![("Music/Some Album/1.flac", t(&["A"], None))]);
        assert_eq!(idx.albums()[0].title, "Some Album");
        assert_eq!(idx.albums()[0].key_folder, "Music/Some Album");
    }

    #[test]
    fn same_album_name_in_two_folders_stays_apart() {
        let idx = build(vec![
            ("2001/1.flac", t(&["A"], Some("Hits"))),
            ("2020/1.flac", t(&["A"], Some("Hits"))),
        ]);
        assert_eq!(idx.albums().len(), 2);
        assert_ne!(idx.albums()[0].id, idx.albums()[1].id);
    }

    #[test]
    fn disc_folders_merge_into_one_album() {
        let idx = build(vec![
            ("A/Album/Disc 2/01.flac", t(&["A"], None)),
            ("A/Album/CD1/02.flac", t(&["A"], None)),
            ("A/Album/CD1/01.flac", t(&["A"], None)),
        ]);
        assert_eq!(idx.albums().len(), 1);
        assert_eq!(
            idx.albums()[0].tracks,
            [
                "A/Album/CD1/01.flac",
                "A/Album/CD1/02.flac",
                "A/Album/Disc 2/01.flac"
            ]
        );
        assert_eq!(idx.albums()[0].key_folder, "A/Album");
    }

    #[test]
    fn disc_folder_detection() {
        for yes in ["CD1", "cd 2", "Disc 10", "disk3"] {
            assert_eq!(album_folder(&format!("A/{yes}/1.flac")), "A", "{yes}");
        }
        for no in ["CDs", "Disco", "Disc", "Disc 2 bonus"] {
            assert_eq!(
                album_folder(&format!("A/{no}/1.flac")),
                format!("A/{no}"),
                "{no}"
            );
        }
        assert_eq!(album_folder("CD1/1.flac"), "");
        assert_eq!(album_folder("1.flac"), "");
    }

    #[test]
    fn unnumbered_tracks_sort_after_numbered() {
        let numbered = |n| TrackTags {
            track: Some(n),
            ..t(&["A"], Some("Al"))
        };
        let idx = build(vec![
            ("d/a.flac", t(&["A"], Some("Al"))),
            ("d/z.flac", numbered(2)),
            ("d/y.flac", numbered(1)),
        ]);
        assert_eq!(idx.albums()[0].tracks, ["d/y.flac", "d/z.flac", "d/a.flac"]);
    }

    #[test]
    fn album_year_is_earliest() {
        let year = |y| TrackTags {
            year: Some(y),
            ..t(&["A"], Some("Al"))
        };
        let idx = build(vec![
            ("d/1.flac", year(2005)),
            ("d/2.flac", t(&["A"], Some("Al"))),
            ("d/3.flac", year(1999)),
        ]);
        assert_eq!(idx.albums()[0].year, Some(1999));
    }

    #[test]
    fn artist_names_fold_width_and_case() {
        let idx = build(vec![
            ("a/1.flac", t(&["ＡＢＣ"], Some("One"))),
            ("b/1.flac", t(&["abc"], Some("Two"))),
            ("b/2.flac", t(&["abc"], Some("Two"))),
        ]);
        assert_eq!(idx.artists().len(), 1);
        assert_eq!(idx.artists()[0].name, "abc");
        assert_eq!(idx.artists()[0].albums.len(), 2);
    }

    #[test]
    fn album_shows_the_library_wide_spelling_of_its_artist() {
        let tag = |aa: &str, album: &str| TrackTags {
            album_artist: Some(aa.into()),
            ..t(&["x"], Some(album))
        };
        let idx = build(vec![
            ("a/1.flac", tag("abc", "A")),
            ("b/1.flac", tag("ＡＢＣ", "B")),
            ("c/1.flac", tag("ＡＢＣ", "C")),
            ("d/1.flac", tag("ＡＢＣ", "D")),
        ]);
        assert_eq!(idx.artists().len(), 1);
        assert_eq!(idx.artists()[0].name, "ＡＢＣ");
        for album in idx.albums() {
            let artist = idx.artist(&album.album_artist_id).unwrap();
            assert_eq!(album.album_artist, artist.name, "{}", album.title);
        }
    }

    #[test]
    fn appears_on_excludes_own_albums() {
        let own = |artists: &[&str]| TrackTags {
            album_artist: Some("Amy".into()),
            ..t(artists, Some("Own"))
        };
        let idx = build(vec![
            ("own/1.flac", own(&["Amy"])),
            ("own/2.flac", own(&["Amy", "Bob"])),
            ("va/1.flac", t(&["Amy"], Some("Mix"))),
            ("va/2.flac", t(&["Bob"], Some("Mix"))),
        ]);
        let by_name = |n: &str| idx.artists().iter().find(|a| a.name == n).unwrap();
        let amy = by_name("Amy");
        let own = idx.albums().iter().find(|a| a.title == "Own").unwrap();
        let mix = idx.albums().iter().find(|a| a.title == "Mix").unwrap();
        assert_eq!(amy.albums, std::slice::from_ref(&own.id));
        assert_eq!(amy.appears_on, std::slice::from_ref(&mix.id));
        // Bob has no album of his own, so he is no entry; Various Artists is one.
        assert!(idx.artists().iter().all(|a| a.name != "Bob"));
        assert_eq!(
            by_name("Various Artists").albums,
            std::slice::from_ref(&mix.id)
        );
    }

    #[test]
    fn sort_tags_order_artists() {
        let tag = |name: &str, sort: Option<&str>| TrackTags {
            album_artist: Some(name.into()),
            album_artist_sort: sort.map(str::to_owned),
            ..t(&[name], Some(name))
        };
        let idx = build(vec![
            ("c/1.flac", tag("Carpenters", None)),
            ("b/1.flac", tag("The Beatles", Some("Beatles, The"))),
        ]);
        let names: Vec<_> = idx.artists().iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["The Beatles", "Carpenters"]);
        let titles: Vec<_> = idx.albums().iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["The Beatles", "Carpenters"]);
    }

    #[test]
    fn embedded_cover_item_is_first_track_with_embedded_cover() {
        let cover = |n| TrackTags {
            track: Some(n),
            has_embedded_cover: true,
            ..t(&["A"], Some("Al"))
        };
        let idx = build(vec![
            ("d/c.flac", cover(3)),
            (
                "d/a.flac",
                TrackTags {
                    track: Some(1),
                    ..t(&["A"], Some("Al"))
                },
            ),
            ("d/b.flac", cover(2)),
        ]);
        assert_eq!(
            idx.albums()[0].embedded_cover_item.as_deref(),
            Some("d/b.flac")
        );
        let idx = build(vec![("d/a.flac", t(&["A"], Some("Al")))]);
        assert_eq!(idx.albums()[0].embedded_cover_item, None);
    }

    #[test]
    fn ids_are_stable_across_builds() {
        let items = || {
            vec![
                ("x/1.flac", t(&["Amy"], Some("One"))),
                ("y/1.flac", t(&["Bob"], Some("Two"))),
            ]
        };
        let (a, b) = (build(items()), build(items()));
        assert_eq!(a.albums(), b.albums());
        assert_eq!(a.artists(), b.artists());
        let first = &a.albums()[0];
        assert_eq!(a.album(&first.id), Some(first));
        assert_eq!(
            a.artist(&first.album_artist_id).map(|x| &x.id),
            Some(&first.album_artist_id)
        );
        let other = LibraryId(Uuid::from_u128(8));
        let c = Index::build(&other, items().into_iter().map(|(r, t)| (r.to_owned(), t)));
        assert_ne!(c.albums()[0].id, first.id);
        assert_eq!(first.id, album_id(&lib(), "x", "one"));
    }

    #[test]
    fn twenty_thousand_tracks_build_quickly() {
        let items: Vec<(String, TrackTags)> = (0..20_000)
            .map(|i| {
                let album = i % 2000;
                let tags = TrackTags {
                    track: Some(i as u32 / 2000),
                    ..t(
                        &[&format!("Artist {}", album % 300)],
                        Some(&format!("Album {album}")),
                    )
                };
                (format!("lib/Album {album}/{i}.flac"), tags)
            })
            .collect();
        let start = std::time::Instant::now();
        let idx = Index::build(&lib(), items);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(idx.albums().len(), 2000);
        assert_eq!(idx.artists().len(), 300);
    }
}
