use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const META_DIR: &str = ".asteria";

const SIDECAR_EXT: &str = "xmp";

/// `a/b.HEIC` -> `a/b.HEIC.xmp`
pub fn sidecar_path_for(media: &Path) -> PathBuf {
    let mut s: OsString = media.as_os_str().to_owned();
    s.push(".");
    s.push(SIDECAR_EXT);
    PathBuf::from(s)
}

pub fn is_sidecar(path: &Path) -> bool {
    media_path_for_sidecar(path).is_some()
}

/// `a/b.HEIC.xmp` -> `Some(a/b.HEIC)`; `None` if `sidecar` is not a sidecar path.
pub fn media_path_for_sidecar(sidecar: &Path) -> Option<PathBuf> {
    let ext = sidecar.extension()?;
    if !ext.eq_ignore_ascii_case(SIDECAR_EXT) {
        return None;
    }
    let media = sidecar.with_extension("");
    // A bare ".xmp" has no media name.
    media.file_name()?;
    Some(media)
}
