//! Filesystem helpers shared by the writers of original files.

use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::path::Path;

/// Whether a failed `hard_link` should be retried as check-then-rename: every error except
/// `AlreadyExists`. Some filesystems (vfat, exFAT) report EPERM for hard links; if the
/// problem is a real permission error the rename fails too and that error is reported.
fn should_fallback(e: &std::io::Error) -> bool {
    e.kind() != ErrorKind::AlreadyExists
}

/// Moves `from` to `to` without ever replacing an existing `to`.
///
/// Uses `hard_link` (fails if `to` exists, so a racing creator is never overwritten)
/// followed by removing `from`. If `hard_link` fails for any other reason it falls back to check-then-`rename`, which is acceptable because all library
/// writes go through the single write queue.
pub(crate) fn move_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            return std::fs::remove_file(from).inspect_err(|_| {
                if let Err(e) = std::fs::remove_file(to) {
                    tracing::warn!(from = %from.display(), to = %to.display(), error = %e,
                        "rollback failed: both link names remain");
                }
            });
        }
        Err(e) if !should_fallback(&e) => return Err(e),
        Err(_) => {}
    }
    if to.symlink_metadata().is_ok() {
        return Err(ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

/// How [`place_no_clobber`] put a file in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placed {
    /// Moved (renamed): `from` is gone.
    Moved,
    /// Copied, since `from` is on another filesystem: `from` is still there, so that a
    /// rollback only has to remove the copy. The caller removes `from` when done.
    Copied,
}

/// Puts `from` at `to` without ever replacing an existing `to`: [`move_no_clobber`] when both
/// are on one filesystem, otherwise (`EXDEV`, or `ERROR_NOT_SAME_DEVICE` on Windows) a copy
/// into a temp file beside `to`, synced and then moved into place the same no-clobber way.
pub(crate) fn place_no_clobber(from: &Path, to: &Path) -> io::Result<Placed> {
    let moved = if test_hook::is_cross_device() {
        Err(ErrorKind::CrossesDevices.into())
    } else {
        move_no_clobber(from, to)
    };
    match moved {
        Ok(()) => Ok(Placed::Moved),
        Err(e) if e.kind() == ErrorKind::CrossesDevices => {
            copy_no_clobber(from, to)?;
            Ok(Placed::Copied)
        }
        Err(e) => Err(e),
    }
}

/// Copies `from` to `to` through a temp file in `to`'s directory, which is removed if
/// anything fails. `from` is left alone.
fn copy_no_clobber(from: &Path, to: &Path) -> io::Result<()> {
    let tmp = asteria_framework::fsutil::tmp_path_for(to);
    let mut src = File::open(from)?;
    let mut dst = File::options().write(true).create_new(true).open(&tmp)?;
    let copied = io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    // Closed before it is moved or removed (Windows will not do either to an open file).
    drop(dst);
    let result = copied.and_then(|()| move_no_clobber(&tmp, to));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Lets a test make every placement look cross-filesystem, on the current thread.
pub(crate) mod test_hook {
    #[cfg(test)]
    thread_local! {
        static CROSS_DEVICE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[cfg(test)]
    pub(crate) fn is_cross_device() -> bool {
        CROSS_DEVICE.with(std::cell::Cell::get)
    }

    #[cfg(not(test))]
    pub(crate) fn is_cross_device() -> bool {
        false
    }

    /// Runs `f` with placements forced onto the copy path.
    #[cfg(test)]
    pub(crate) fn cross_device<T>(f: impl FnOnce() -> T) -> T {
        CROSS_DEVICE.with(|c| c.set(true));
        let out = f();
        CROSS_DEVICE.with(|c| c.set(false));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    /// `from` in one directory and the target directory `to_dir`, as for an upload commit.
    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let (src, dst) = (d.path().join("state"), d.path().join("lib"));
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.join("data"), b"payload").unwrap();
        (d, src.join("data"), dst)
    }

    #[test]
    fn same_filesystem_placement_moves() {
        let (_d, from, dst) = dirs();
        let to = dst.join("a.jpg");
        assert_eq!(place_no_clobber(&from, &to).unwrap(), Placed::Moved);
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"payload");
    }

    #[test]
    fn cross_device_placement_copies_and_keeps_the_source() {
        let (_d, from, dst) = dirs();
        let to = dst.join("a.jpg");
        let placed = test_hook::cross_device(|| place_no_clobber(&from, &to)).unwrap();
        assert_eq!(placed, Placed::Copied);
        assert_eq!(std::fs::read(&to).unwrap(), b"payload");
        // The caller drops the source once the whole group is in place.
        assert_eq!(std::fs::read(&from).unwrap(), b"payload");
        assert_eq!(names(&dst), ["a.jpg"]);
    }

    #[test]
    fn cross_device_placement_never_clobbers() {
        let (_d, from, dst) = dirs();
        let to = dst.join("a.jpg");
        std::fs::write(&to, b"theirs").unwrap();
        let err = test_hook::cross_device(|| place_no_clobber(&from, &to)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&to).unwrap(), b"theirs");
        assert!(from.exists());
        assert_eq!(names(&dst), ["a.jpg"]);
    }

    #[test]
    fn failed_cross_device_copy_leaves_no_temp_file() {
        let (_d, from, dst) = dirs();
        std::fs::remove_file(&from).unwrap();
        let to = dst.join("a.jpg");
        assert!(test_hook::cross_device(|| place_no_clobber(&from, &to)).is_err());
        assert!(names(&dst).is_empty());
    }

    #[test]
    fn fallback_decision() {
        use std::io::{Error, ErrorKind::*};
        assert!(!should_fallback(&Error::from(AlreadyExists)));
        assert!(should_fallback(&Error::from(PermissionDenied)));
        assert!(should_fallback(&Error::from(Unsupported)));
    }
}
