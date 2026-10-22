//! Coarse media kind classification: magic bytes first, extension as the fallback.

use std::path::Path;

use crate::Result;
use crate::hash::{MediaFormat, detect_format_file};
use asteria_framework_api::MediaKind;

const IMAGE: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "avif", "tif", "tiff", "dng", "cr2",
    "cr3", "nef", "arw", "orf", "rw2", "raf",
];
const VIDEO: &[&str] = &["mp4", "mov", "m4v", "mkv", "webm", "avi", "3gp"];
const AUDIO: &[&str] = &["flac", "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav"];

/// Classifies `path`. JPEG/PNG/WebP content is `Image` and FLAC/MP3 content is `Audio`
/// whatever the extension says; otherwise the (case-insensitive) extension decides.
pub fn classify(path: &Path) -> Result<MediaKind> {
    match detect_format_file(path)? {
        MediaFormat::Jpeg | MediaFormat::Png | MediaFormat::WebP => return Ok(MediaKind::Image),
        MediaFormat::Flac | MediaFormat::Mp3 => return Ok(MediaKind::Audio),
        _ => {}
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ext = ext.as_str();
    Ok(if IMAGE.contains(&ext) {
        MediaKind::Image
    } else if VIDEO.contains(&ext) {
        MediaKind::Video
    } else if AUDIO.contains(&ext) {
        MediaKind::Audio
    } else {
        MediaKind::Other
    })
}
