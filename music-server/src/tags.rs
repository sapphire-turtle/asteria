//! Reading the embedded tags of a track with lofty.

use std::path::Path;

use asteria_music_api::TrackTags;
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag};

/// Reads the tags of `abs`. A file that cannot be read gives default tags with `tag_error`
/// set to the kind of the failure (never a path), so the scan can carry on.
pub fn read(abs: &Path) -> TrackTags {
    let file = match lofty::read_from_path(abs) {
        Ok(file) => file,
        Err(e) => {
            return TrackTags {
                tag_error: Some(format!("unreadable tags: {:?}", e.kind())),
                ..TrackTags::default()
            };
        }
    };
    let duration = file.properties().duration().as_millis();
    let mut out = file
        .primary_tag()
        .or_else(|| file.first_tag())
        .map(from_tag)
        .unwrap_or_default();
    out.duration_ms = u64::try_from(duration).ok();
    out
}

fn from_tag(tag: &Tag) -> TrackTags {
    let text = |key: ItemKey| {
        tag.get_string(&key)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    TrackTags {
        title: text(ItemKey::TrackTitle),
        // Every value in the order it appears; no splitting on `;` or the like.
        artists: strings(tag, ItemKey::TrackArtist),
        album: text(ItemKey::AlbumTitle),
        album_artist: text(ItemKey::AlbumArtist),
        artist_sort: text(ItemKey::TrackArtistSortOrder),
        album_artist_sort: text(ItemKey::AlbumArtistSortOrder),
        album_sort: text(ItemKey::AlbumTitleSortOrder),
        track: tag.track(),
        track_total: tag.track_total(),
        disc: tag.disk(),
        disc_total: tag.disk_total(),
        year: text(ItemKey::RecordingDate)
            .or_else(|| text(ItemKey::Year))
            .and_then(|d| leading_year(&d)),
        genres: strings(tag, ItemKey::Genre),
        duration_ms: None,
        has_embedded_cover: !tag.pictures().is_empty(),
        tag_error: None,
    }
}

fn strings(tag: &Tag, key: ItemKey) -> Vec<String> {
    tag.get_strings(&key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The year of a date such as `1999`, `1999-05` or `1999-05-01T10:00`.
fn leading_year(date: &str) -> Option<i32> {
    let digits = date.get(..4)?;
    digits
        .chars()
        .all(|c| c.is_ascii_digit())
        .then(|| digits.parse().ok())?
}

/// The title to show: the title tag, or the file name without its extension.
#[cfg_attr(not(test), expect(dead_code, reason = "used by the music RPC methods"))]
pub fn display_title(tags: &TrackTags, rel: &str) -> String {
    if let Some(title) = &tags.title {
        return title.clone();
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    Path::new(name)
        .file_stem()
        .map_or_else(|| name.to_owned(), |s| s.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{TagItem, TagType};
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/asteria-framework/tests/fixtures")
            .join(name)
    }

    /// Copies a fixture into `dir` and writes `tag` into the copy.
    fn tagged(dir: &Path, name: &str, tag: Tag) -> PathBuf {
        let path = dir.join(name);
        std::fs::copy(fixture(name), &path).unwrap();
        let mut file = lofty::read_from_path(&path).unwrap();
        file.insert_tag(tag);
        file.save_to_path(&path, WriteOptions::default()).unwrap();
        path
    }

    fn push(tag: &mut Tag, key: ItemKey, value: &str) {
        tag.push(TagItem::new(key, lofty::tag::ItemValue::Text(value.into())));
    }

    fn full_tag(tag_type: TagType) -> Tag {
        let mut tag = Tag::new(tag_type);
        push(&mut tag, ItemKey::TrackTitle, "A Song");
        push(&mut tag, ItemKey::TrackArtist, "First; Second");
        push(&mut tag, ItemKey::TrackArtist, "Third");
        push(&mut tag, ItemKey::AlbumTitle, "An Album");
        push(&mut tag, ItemKey::AlbumArtist, "Various");
        push(&mut tag, ItemKey::TrackNumber, "3");
        push(&mut tag, ItemKey::DiscNumber, "2");
        push(&mut tag, ItemKey::RecordingDate, "1999");
        push(&mut tag, ItemKey::Genre, "Rock");
        tag
    }

    #[test]
    fn reads_vorbis_comments_from_flac() {
        let dir = tempfile::tempdir().unwrap();
        let p = tagged(
            dir.path(),
            "sine440.flac",
            full_tag(TagType::VorbisComments),
        );
        let t = read(&p);
        assert_eq!(t.tag_error, None);
        assert_eq!(t.title.as_deref(), Some("A Song"));
        // Two ARTIST fields stay two; the `;` inside one is not a separator.
        assert_eq!(t.artists, ["First; Second", "Third"]);
        assert_eq!(t.album.as_deref(), Some("An Album"));
        assert_eq!(t.album_artist.as_deref(), Some("Various"));
        assert_eq!(t.track, Some(3));
        assert_eq!(t.disc, Some(2));
        assert_eq!(t.year, Some(1999));
        assert_eq!(t.genres, ["Rock"]);
        assert!(t.duration_ms.is_some_and(|d| d > 0));
        assert!(!t.has_embedded_cover);
    }

    #[test]
    fn reads_id3v2_from_mp3() {
        let dir = tempfile::tempdir().unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        push(&mut tag, ItemKey::TrackTitle, "An MP3");
        push(&mut tag, ItemKey::TrackArtist, "Someone");
        push(&mut tag, ItemKey::AlbumTitle, "Album");
        push(&mut tag, ItemKey::TrackNumber, "7");
        push(&mut tag, ItemKey::RecordingDate, "2001-05-06");
        let p = tagged(dir.path(), "sine440.mp3", tag);
        let t = read(&p);
        assert_eq!(t.tag_error, None);
        assert_eq!(t.title.as_deref(), Some("An MP3"));
        assert_eq!(t.artists, ["Someone"]);
        assert_eq!(t.album.as_deref(), Some("Album"));
        assert_eq!(t.track, Some(7));
        assert_eq!(t.year, Some(2001));
        assert!(t.duration_ms.is_some_and(|d| d > 0));
    }

    #[test]
    fn embedded_cover_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let mut tag = full_tag(TagType::VorbisComments);
        tag.push_picture(Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Png),
            None,
            vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4, 5, 6, 7, 8],
        ));
        let p = tagged(dir.path(), "sine440.flac", tag);
        assert!(read(&p).has_embedded_cover);
    }

    #[test]
    fn untagged_file_gives_defaults() {
        let t = read(&fixture("sine440.flac"));
        assert_eq!(t.title, None);
        assert!(t.artists.is_empty());
        assert_eq!(t.tag_error, None);
        assert!(t.duration_ms.is_some());
    }

    #[test]
    fn garbage_file_sets_tag_error_without_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("garbage.flac");
        let bytes: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(&p, bytes).unwrap();
        let t = read(&p);
        let err = t.tag_error.expect("garbage must set tag_error");
        assert!(err.starts_with("unreadable tags: "), "{err}");
        let dir_name = dir.path().file_name().unwrap().to_str().unwrap();
        assert!(!err.contains(dir_name), "{err}");
        assert!(!err.contains("garbage"), "{err}");
        assert!(!err.contains('/'), "{err}");
        assert_eq!(t.title, None);
    }

    #[test]
    fn empty_artist_values_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut tag = Tag::new(TagType::VorbisComments);
        push(&mut tag, ItemKey::TrackArtist, "   ");
        push(&mut tag, ItemKey::TrackArtist, "  Real Artist ");
        push(&mut tag, ItemKey::TrackArtist, "");
        let p = tagged(dir.path(), "sine440.flac", tag);
        assert_eq!(read(&p).artists, ["Real Artist"]);
    }

    #[test]
    fn display_title_falls_back_to_stem() {
        let none = TrackTags::default();
        assert_eq!(display_title(&none, "Artist/Album/01 Song.flac"), "01 Song");
        let titled = TrackTags {
            title: Some("Real".into()),
            ..TrackTags::default()
        };
        assert_eq!(display_title(&titled, "Artist/Album/01 Song.flac"), "Real");
    }
}
