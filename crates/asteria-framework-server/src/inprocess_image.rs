//! Image thumbnails made in the server's own process with `corundum-framework-image`.
//!
//! The decode is bounded by [`Limits::default`] and runs on a blocking thread (the caller
//! uses `spawn_blocking`); the WebP bytes it returns are written by the async side, so a
//! render that timed out never writes a file after its cleanup. What the crate cannot read
//! (HEIC, AVIF, ORF, a picture over the limits) is an [`Error`] the caller hands to an
//! external tool.

use std::path::PathBuf;

use corundum_framework_image::{Error, Limits, Source, encode_webp, thumbnail};

/// The WebP quality of every thumbnail, as with the external tools.
const QUALITY: f32 = 80.0;

/// An image to render, owned so it can move into `spawn_blocking`.
pub(crate) enum InSource {
    Path(PathBuf),
    Bytes { bytes: Vec<u8>, ext: Option<String> },
}

/// The image shrunk so its longest side is at most `longest` pixels (never enlarged), with
/// its EXIF orientation applied, as lossy WebP of quality 80 without metadata.
pub(crate) fn render(src: InSource, longest: u32) -> Result<Vec<u8>, Error> {
    let limits = Limits::default();
    let image = match &src {
        InSource::Path(path) => thumbnail(Source::Path(path), longest, &limits)?,
        InSource::Bytes { bytes, ext } => thumbnail(
            Source::Bytes {
                bytes,
                ext: ext.as_deref(),
            },
            longest,
            &limits,
        )?,
    };
    encode_webp(&image, QUALITY)
}
