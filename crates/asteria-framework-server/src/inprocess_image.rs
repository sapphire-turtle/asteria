//! Image thumbnails made in the server's own process with `corundum-framework-image`.
//!
//! The decode is bounded by [`LIMITS`] and runs on a blocking thread (the caller
//! uses `spawn_blocking`); the WebP bytes it returns are written by the async side, so a
//! render that timed out never writes a file after its cleanup. What the crate cannot read
//! (HEIC, AVIF, ORF, a picture over the limits) is an [`Error`] the caller hands to an
//! external tool.

use std::path::PathBuf;

use corundum_framework_image::{Error, Limits, Source, encode_webp, thumbnail};

/// The WebP quality of every thumbnail, as with the external tools.
const QUALITY: f32 = 80.0;

/// The most pixels the server decodes in its own process. A decode holds the whole picture
/// (several bytes a pixel, plus the shrinking), and two run at once, so the crate's default of
/// 200 megapixels could take 2-3 GB each and get the server killed on a 2-4 GB NAS or
/// Raspberry Pi. Larger pictures go to the external tool, which streams them.
const MAX_PIXELS: u64 = 64_000_000;

/// The most bytes of file the server decodes in its own process (see [`MAX_PIXELS`]).
const MAX_BYTES: u64 = 256 << 20;

/// The limits of an in-process decode: the crate's defaults, tightened for small home servers.
const LIMITS: Limits = Limits::new(MAX_BYTES, MAX_PIXELS);

/// An image to render, owned so it can move into `spawn_blocking`.
pub(crate) enum InSource {
    Path(PathBuf),
    Bytes { bytes: Vec<u8>, ext: Option<String> },
}

/// The image shrunk so its longest side is at most `longest` pixels (never enlarged), with
/// its EXIF orientation applied, as lossy WebP of quality 80 without metadata.
pub(crate) fn render(src: InSource, longest: u32) -> Result<Vec<u8>, Error> {
    let limits = LIMITS;
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
