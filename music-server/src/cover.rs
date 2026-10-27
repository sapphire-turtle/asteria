//! Album art: a track's embedded picture, else an image file in its folder.
#![cfg_attr(not(test), expect(dead_code, reason = "used by the music extension"))]

use std::path::{Component, Path};

use asteria_framework_server::ThumbSource;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::*;

use crate::index::album_folder;

pub const NAMES: [&str; 4] = ["cover", "folder", "front", "album"];
pub const EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// True for a file name `<NAMES>.<EXTS>` (ASCII case-insensitive). Used by `wants_extra`.
pub fn is_cover_name(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| NAMES.contains(&stem) && EXTS.contains(&ext))
}

/// The file extension a picture's MIME type needs for decoding.
fn ext_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "bin",
    }
}

/// Embedded picture: front cover first, else the first picture. `None` if none or unreadable.
pub fn embedded(abs: &Path) -> Option<(Vec<u8>, &'static str)> {
    let file = lofty::read_from_path(abs).ok()?;
    let pictures: Vec<&Picture> = file.tags().iter().flat_map(|t| t.pictures()).collect();
    let pic = pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())?;
    if pic.data().is_empty() {
        return None;
    }
    let ext = pic.mime_type().map_or("bin", |m| ext_for_mime(m.as_str()));
    Some((pic.data().to_vec(), ext))
}

/// The folder image for the track at `rel`, from the extras' paths: track folder first, then
/// the album folder (`index::album_folder`); within a folder, NAMES order then EXTS order.
pub fn folder_image<'a>(rel: &str, extras: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let track_dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let album_dir = album_folder(rel);
    let extras: Vec<&str> = extras.into_iter().collect();
    let mut dirs = vec![track_dir];
    if album_dir != track_dir {
        dirs.push(album_dir);
    }
    for dir in dirs {
        for name in NAMES {
            for ext in EXTS {
                let wanted = format!("{name}.{ext}");
                let found = extras.iter().find(|e| {
                    let (d, f) = e.rsplit_once('/').unwrap_or(("", e));
                    d == dir && f.to_ascii_lowercase() == wanted
                });
                if let Some(found) = found {
                    return Some((*found).to_string());
                }
            }
        }
    }
    None
}

fn key(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// Reads `root/img` unless it resolves (through symlinks) outside the library or into a
/// hidden path.
fn read_folder_image(root: &Path, img: &str) -> Option<(String, std::path::PathBuf)> {
    let path = root.join(img);
    let canon_root = root.canonicalize().ok()?;
    let canon = path.canonicalize().ok()?;
    let inside = canon.strip_prefix(&canon_root).ok()?;
    let clean = inside
        .components()
        .all(|c| matches!(c, Component::Normal(n) if !n.to_string_lossy().starts_with('.')));
    if !clean {
        return None;
    }
    let bytes = std::fs::read(&canon).ok()?;
    Some((key(&bytes), path))
}

pub fn thumb_source(root: &Path, rel: &str, abs: &Path, extras: &[String]) -> ThumbSource {
    if let Some((bytes, ext)) = embedded(abs) {
        return ThumbSource::Image {
            key: key(&bytes),
            bytes,
            ext,
        };
    }
    let Some(img) = folder_image(rel, extras.iter().map(String::as_str)) else {
        return ThumbSource::None;
    };
    match read_folder_image(root, &img) {
        Some((key, path)) => ThumbSource::File { key, path },
        None => ThumbSource::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::config::WriteOptions;
    use lofty::picture::MimeType;
    use lofty::tag::{Tag, TagType};

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/asteria-framework/tests/fixtures")
            .join(name)
    }

    /// Copies sine440.flac to `dir/name` with the given pictures embedded.
    fn track_with(
        dir: &Path,
        name: &str,
        pics: &[(PictureType, MimeType, &[u8])],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::copy(fixture("sine440.flac"), &path).unwrap();
        let mut file = lofty::read_from_path(&path).unwrap();
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.set_title("T".to_string());
        for (ty, mime, data) in pics {
            tag.push_picture(Picture::new_unchecked(
                *ty,
                Some(mime.clone()),
                None,
                data.to_vec(),
            ));
        }
        file.insert_tag(tag);
        file.save_to_path(&path, WriteOptions::default()).unwrap();
        path
    }

    fn plain_track(dir: &Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::copy(fixture("sine440.flac"), &path).unwrap();
        path
    }

    #[test]
    fn cover_names() {
        assert!(is_cover_name("Cover.JPG"));
        assert!(is_cover_name("folder.png"));
        assert!(!is_cover_name("cover.gif"));
        assert!(!is_cover_name("covers.jpg"));
        assert!(!is_cover_name("back.jpg"));
    }

    #[test]
    fn front_cover_preferred_over_other_pictures() {
        let dir = tempfile::tempdir().unwrap();
        let p = track_with(
            dir.path(),
            "a.flac",
            &[
                (PictureType::Artist, MimeType::Jpeg, b"artist-bytes"),
                (PictureType::CoverFront, MimeType::Png, b"front-bytes"),
            ],
        );
        let (bytes, ext) = embedded(&p).unwrap();
        assert_eq!(bytes, b"front-bytes");
        assert_eq!(ext, "png");
    }

    #[test]
    fn first_picture_when_no_front_cover() {
        let dir = tempfile::tempdir().unwrap();
        let p = track_with(
            dir.path(),
            "a.flac",
            &[
                (PictureType::Artist, MimeType::Jpeg, b"one"),
                (PictureType::Other, MimeType::Gif, b"two"),
            ],
        );
        assert_eq!(embedded(&p).unwrap(), (b"one".to_vec(), "jpg"));
    }

    #[test]
    fn embedded_is_none_for_unreadable_or_pictureless_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(embedded(&dir.path().join("missing.flac")).is_none());
        let junk = dir.path().join("junk.flac");
        std::fs::write(&junk, b"not audio").unwrap();
        assert!(embedded(&junk).is_none());
        assert!(embedded(&plain_track(dir.path(), "p.flac")).is_none());
    }

    #[test]
    fn track_folder_beats_album_folder() {
        let got = folder_image(
            "A/Album/CD1/01.flac",
            ["A/Album/folder.jpg", "A/Album/CD1/cover.jpg"],
        );
        assert_eq!(got.as_deref(), Some("A/Album/CD1/cover.jpg"));
        let got = folder_image("A/Album/CD1/01.flac", ["A/Album/folder.jpg"]);
        assert_eq!(got.as_deref(), Some("A/Album/folder.jpg"));
    }

    #[test]
    fn names_order_within_a_folder() {
        let got = folder_image("A/01.flac", ["A/folder.jpg", "A/cover.png"]);
        assert_eq!(got.as_deref(), Some("A/cover.png"));
        let got = folder_image("A/01.flac", ["A/cover.png", "A/Cover.JPG"]);
        assert_eq!(got.as_deref(), Some("A/Cover.JPG"));
    }

    #[test]
    fn embedded_beats_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"folder").unwrap();
        track_with(
            dir.path(),
            "a.flac",
            &[(PictureType::CoverFront, MimeType::Png, b"embedded")],
        );
        let src = thumb_source(
            dir.path(),
            "a.flac",
            &dir.path().join("a.flac"),
            &["cover.jpg".to_string()],
        );
        assert_eq!(
            src,
            ThumbSource::Image {
                key: key(b"embedded"),
                bytes: b"embedded".to_vec(),
                ext: "png"
            }
        );
    }

    #[test]
    fn folder_image_gives_file_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"folder").unwrap();
        plain_track(dir.path(), "a.flac");
        let src = thumb_source(
            dir.path(),
            "a.flac",
            &dir.path().join("a.flac"),
            &["cover.jpg".to_string()],
        );
        assert_eq!(
            src,
            ThumbSource::File {
                key: key(b"folder"),
                path: dir.path().join("cover.jpg")
            }
        );
    }

    #[test]
    fn same_image_gives_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let pics = [(PictureType::CoverFront, MimeType::Png, &b"same"[..])];
        let a = track_with(dir.path(), "a.flac", &pics);
        let b = track_with(dir.path(), "b.flac", &pics);
        let ka = thumb_source(dir.path(), "a.flac", &a, &[]);
        let kb = thumb_source(dir.path(), "b.flac", &b, &[]);
        assert_eq!(ka, kb);
        assert!(matches!(ka, ThumbSource::Image { ref key, .. } if key.starts_with("blake3:")));
    }

    #[test]
    fn nothing_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        let a = plain_track(dir.path(), "a.flac");
        assert_eq!(
            thumb_source(dir.path(), "a.flac", &a, &[]),
            ThumbSource::None
        );
        // An extra that is listed but missing on disk is also nothing.
        let extras = ["cover.jpg".to_string()];
        assert_eq!(
            thumb_source(dir.path(), "a.flac", &a, &extras),
            ThumbSource::None
        );
    }

    #[cfg(unix)]
    #[test]
    fn folder_image_symlink_outside_library_is_ignored() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.jpg"), b"secret").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.jpg"),
            root.path().join("cover.jpg"),
        )
        .unwrap();
        let a = plain_track(root.path(), "a.flac");
        let extras = ["cover.jpg".to_string()];
        assert_eq!(
            thumb_source(root.path(), "a.flac", &a, &extras),
            ThumbSource::None
        );
    }

    #[cfg(unix)]
    #[test]
    fn folder_image_symlink_inside_library_is_used_unless_hidden() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".hidden")).unwrap();
        std::fs::write(root.path().join(".hidden/pic.jpg"), b"x").unwrap();
        std::os::unix::fs::symlink(".hidden/pic.jpg", root.path().join("cover.jpg")).unwrap();
        let a = plain_track(root.path(), "a.flac");
        let extras = ["cover.jpg".to_string()];
        assert_eq!(
            thumb_source(root.path(), "a.flac", &a, &extras),
            ThumbSource::None
        );
    }
}
