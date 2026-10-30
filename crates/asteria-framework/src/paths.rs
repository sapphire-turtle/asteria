use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const META_DIR: &str = ".asteria";

/// Holds trashed files, one folder per op, under [`META_DIR`].
pub const TRASH_DIR: &str = "trash";

/// Library-relative path a trashed file is moved to: `.asteria/trash/<op-id>/<rel>`.
pub fn trash_rel(op_id: &crate::journal::OpId, rel: &str) -> String {
    format!("{META_DIR}/{TRASH_DIR}/{}/{rel}", op_id.0)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use grain_id::GrainId;

    #[test]
    fn trash_rel_nests_under_the_op_id() {
        let g = GrainId::random();
        assert_eq!(
            trash_rel(&crate::journal::OpId(g), "2025/a.jpg"),
            format!(".asteria/trash/{g}/2025/a.jpg")
        );
    }
}
