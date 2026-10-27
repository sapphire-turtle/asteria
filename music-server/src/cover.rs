//! Album art: a track's embedded picture, else an image file in its folder.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

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

/// The folder images for the track at `rel`, best first, from the extras' paths: track folder
/// first, then the album folder (`index::album_folder`); within a folder, NAMES order then
/// EXTS order.
fn folder_candidates<'a>(rel: &str, extras: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let track_dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let album_dir = album_folder(rel);
    let extras: Vec<&str> = extras.into_iter().collect();
    let mut dirs = vec![track_dir];
    if album_dir != track_dir {
        dirs.push(album_dir);
    }
    let mut out = Vec::new();
    for dir in dirs {
        for name in NAMES {
            for ext in EXTS {
                let wanted = format!("{name}.{ext}");
                out.extend(
                    extras
                        .iter()
                        .filter(|e| {
                            let (d, f) = e.rsplit_once('/').unwrap_or(("", e));
                            d == dir && f.to_ascii_lowercase() == wanted
                        })
                        .map(|e| (*e).to_string()),
                );
            }
        }
    }
    out
}

/// The best folder image for the track at `rel`, from the extras' paths.
pub fn folder_image<'a>(rel: &str, extras: impl IntoIterator<Item = &'a str>) -> Option<String> {
    folder_candidates(rel, extras).into_iter().next()
}

fn key(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// A folder image larger than this is not used.
const MAX_FOLDER_IMAGE: u64 = 32 * 1024 * 1024;

/// The canonical path of `root/rel`, unless it resolves (through symlinks) outside the
/// library or into a hidden path. Open the returned path, not `root/rel`.
pub fn canonical_in_library(root: &Path, rel: &str) -> Option<PathBuf> {
    let canon_root = root.canonicalize().ok()?;
    let canon = root.join(rel).canonicalize().ok()?;
    let inside = canon.strip_prefix(&canon_root).ok()?;
    let clean = inside
        .components()
        .all(|c| matches!(c, Component::Normal(n) if !n.to_string_lossy().starts_with('.')));
    clean.then_some(canon)
}

/// Reads `root/img` unless it resolves (through symlinks) outside the library, into a hidden
/// path, or is too big. The bytes come from the canonical path that passed the check.
fn read_folder_image(root: &Path, img: &str) -> Option<(Vec<u8>, &'static str)> {
    let canon = canonical_in_library(root, img)?;
    let ext = match canon.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "jpg" => "jpg",
        "jpeg" => "jpeg",
        "png" => "png",
        "webp" => "webp",
        _ => return None,
    };
    let mut bytes = Vec::new();
    std::fs::File::open(&canon)
        .ok()?
        .take(MAX_FOLDER_IMAGE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_FOLDER_IMAGE {
        return None;
    }
    Some((bytes, ext))
}

pub fn thumb_source(root: &Path, rel: &str, abs: &Path, extras: &[String]) -> ThumbSource {
    let from_bytes = |bytes: Vec<u8>, ext| ThumbSource::Image {
        key: key(&bytes),
        bytes,
        ext,
    };
    if let Some((bytes, ext)) = embedded(abs) {
        return from_bytes(bytes, ext);
    }
    folder_candidates(rel, extras.iter().map(String::as_str))
        .iter()
        .find_map(|img| read_folder_image(root, img))
        .map_or(ThumbSource::None, |(bytes, ext)| from_bytes(bytes, ext))
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
    fn folder_image_gives_image_source() {
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
            ThumbSource::Image {
                key: key(b"folder"),
                bytes: b"folder".to_vec(),
                ext: "jpg"
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

    #[test]
    fn oversized_folder_image_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let big = std::fs::File::create(dir.path().join("cover.jpg")).unwrap();
        big.set_len(33 * 1024 * 1024).unwrap();
        std::fs::write(dir.path().join("folder.png"), b"small").unwrap();
        let a = plain_track(dir.path(), "a.flac");
        let extras = ["cover.jpg".to_string(), "folder.png".to_string()];
        assert_eq!(
            thumb_source(dir.path(), "a.flac", &a, &extras),
            ThumbSource::Image {
                key: key(b"small"),
                bytes: b"small".to_vec(),
                ext: "png"
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn folder_image_symlink_outside_library_falls_through() {
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
        std::fs::write(root.path().join("folder.png"), b"ok").unwrap();
        let extras = ["cover.jpg".to_string(), "folder.png".to_string()];
        assert_eq!(
            thumb_source(root.path(), "a.flac", &a, &extras),
            ThumbSource::Image {
                key: key(b"ok"),
                bytes: b"ok".to_vec(),
                ext: "png"
            }
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
