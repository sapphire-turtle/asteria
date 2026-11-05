//! Helpers for making image thumbnails with macOS's built-in `sips`: it scales and converts
//! to JPEG, and the rest (the EXIF orientation, WebP) is finished here in Rust.

use image::{DynamicImage, ImageDecoder, codecs::jpeg::JpegDecoder};

/// The width and height `sips -g pixelWidth -g pixelHeight` printed, or `None` if either is
/// missing.
pub(crate) fn parse_dimensions(stdout: &str) -> Option<(u32, u32)> {
    let value = |key: &str| {
        stdout.lines().find_map(|line| {
            line.trim()
                .strip_prefix(key)?
                .strip_prefix(':')?
                .trim()
                .parse::<u32>()
                .ok()
        })
    };
    Some((value("pixelWidth")?, value("pixelHeight")?))
}

/// The `sips -Z` value: `longest`, but never more than the image's own longer side, because
/// `-Z` would enlarge a smaller image.
pub(crate) fn target_size(width: u32, height: u32, longest: u32) -> u32 {
    longest.min(width.max(height))
}

/// Turns the JPEG `sips` wrote into lossy WebP at `quality`, with the EXIF orientation
/// applied (`sips` keeps the tag and leaves the pixels as they were).
pub(crate) fn finish(jpeg: &[u8], quality: f32) -> Result<Vec<u8>, String> {
    let mut decoder = JpegDecoder::new(std::io::Cursor::new(jpeg)).map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    image.apply_orientation(orientation);
    // The encoder takes only 8-bit RGB(A).
    let image = DynamicImage::ImageRgb8(image.to_rgb8());
    let encoder = webp::Encoder::from_image(&image).map_err(|e| e.to_string())?;
    Ok(encoder.encode(quality).to_vec())
}

#[cfg(test)]
/// A JPEG of the given size with an EXIF segment holding `orientation`.
pub(crate) fn jpeg_with_orientation(width: u32, height: u32, orientation: u8) -> Vec<u8> {
    let mut jpeg = Vec::new();
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&image)
        .unwrap();
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II*\0");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    // Tag 0x0112 (Orientation), type SHORT, count 1, value.
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&[orientation, 0, 0, 0]);
    tiff.extend_from_slice(&0u32.to_le_bytes());
    let mut segment = b"Exif\0\0".to_vec();
    segment.extend_from_slice(&tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&(segment.len() as u16 + 2).to_be_bytes());
    out.extend_from_slice(&segment);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sips_dimensions_are_read() {
        let out = "/tmp/x.heic\n  pixelWidth: 6000\n  pixelHeight: 4000\n";
        assert_eq!(parse_dimensions(out), Some((6000, 4000)));
        assert_eq!(parse_dimensions("Error 13"), None);
    }

    #[test]
    fn the_size_never_enlarges() {
        assert_eq!(target_size(6000, 4000, 256), 256);
        assert_eq!(target_size(400, 200, 1920), 400);
        assert_eq!(target_size(200, 400, 256), 256);
    }

    #[test]
    fn finishing_applies_the_exif_orientation() {
        let webp = finish(&jpeg_with_orientation(400, 200, 6), 80.0).unwrap();
        let img = image::load_from_memory_with_format(&webp, image::ImageFormat::WebP).unwrap();
        assert_eq!((img.width(), img.height()), (200, 400));
    }

    #[test]
    fn finishing_garbage_is_an_error() {
        assert!(finish(b"not a jpeg", 80.0).is_err());
    }
}
