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

/// How [`stage`] put a file's content on the target filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Staged {
    /// A hard link to the source, on the same filesystem.
    Linked,
    /// A synced copy, made since the source is on another filesystem or the target's
    /// filesystem has no hard links.
    Copied,
}

/// Whether `e` says that source and target are on different filesystems (`EXDEV`, and
/// `ERROR_NOT_SAME_DEVICE` on Windows, which std both reports as `CrossesDevices`).
pub(crate) fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == ErrorKind::CrossesDevices
}

/// Puts the content of `from` at the new file `tmp`, so that it can later be renamed into
/// place on `tmp`'s filesystem: a hard link where possible, otherwise a copy synced to disk.
/// `from` is left alone. Fails with `AlreadyExists`, before copying anything, when `tmp` is
/// taken; on any failure nothing is left at `tmp`.
///
/// The staged file's mtime is now (a hard link would otherwise carry the session data's), so
/// the 24-hour temp sweep of another instance sharing the workspace never takes it mid-commit.
pub(crate) fn stage(from: &Path, tmp: &Path) -> io::Result<Staged> {
    let staged = stage_content(from, tmp)?;
    let touched = File::options()
        .write(true)
        .open(tmp)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    if let Err(e) = touched {
        let _ = fs::remove_file(tmp);
        return Err(e);
    }
    Ok(staged)
}

fn stage_content(from: &Path, tmp: &Path) -> io::Result<Staged> {
    let linked = if test_hook::is_cross_device() {
        Err(ErrorKind::CrossesDevices.into())
    } else {
        fs::hard_link(from, tmp)
    };
    match linked {
        Ok(()) => Ok(Staged::Linked),
        Err(e) if matches!(e.kind(), ErrorKind::AlreadyExists | ErrorKind::NotFound) => Err(e),
        // Another filesystem (or one without hard links, such as vfat): copy.
        Err(e) => {
            if !is_cross_device(&e) {
                tracing::debug!(path = %tmp.display(), error = %e, "no hard link; copying instead");
            }
            copy_new(from, tmp).map(|()| Staged::Copied)
        }
    }
}

/// Copies `from` into the new file `tmp` and syncs it; `tmp` is removed if anything fails
/// after it was created.
fn copy_new(from: &Path, tmp: &Path) -> io::Result<()> {
    let mut src = File::open(from)?;
    let mut dst = File::options().write(true).create_new(true).open(tmp)?;
    let copied = io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    // Closed before it is removed (Windows will not remove an open file).
    drop(dst);
    if copied.is_err() {
        let _ = fs::remove_file(tmp);
    }
    copied
}

/// Persists the entries of `dir` (renames into it); best effort, and only where the
/// platform can sync a directory.
pub(crate) fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Lets a test make every staging look cross-filesystem, on the current thread.
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

    /// Runs `f` with staging forced onto the copy path; the switch is reset however `f`
    /// ends, a panic included.
    #[cfg(test)]
    pub(crate) fn cross_device<T>(f: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                CROSS_DEVICE.with(|c| c.set(false));
            }
        }
        CROSS_DEVICE.with(|c| c.set(true));
        let _reset = Reset;
        f()
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

    /// `from` in a session directory and the target directory `dst`, as for an upload commit,
    /// and the staged file's path in `dst`.
    fn dirs() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let d = tempfile::tempdir().unwrap();
        let (src, dst) = (d.path().join("state"), d.path().join("lib"));
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.join("data"), b"payload").unwrap();
        let tmp = dst.join(".session.tag.asteria-tmp");
        (d, src.join("data"), dst, tmp)
    }

    #[test]
    fn same_filesystem_staging_links() {
        let (_d, from, dst, tmp) = dirs();
        assert_eq!(stage(&from, &tmp).unwrap(), Staged::Linked);
        assert_eq!(std::fs::read(&tmp).unwrap(), b"payload");
        assert_eq!(std::fs::read(&from).unwrap(), b"payload");
        assert_eq!(names(&dst), [".session.tag.asteria-tmp"]);
    }

    /// A staged file is fresh even when its session data is old (a hard link shares the
    /// data's mtime), so the 24-hour temp sweep of another instance leaves it alone.
    #[test]
    fn a_staged_file_is_fresh() {
        use std::time::{Duration, SystemTime};
        let (_d, from, _dst, tmp) = dirs();
        let old = SystemTime::now() - Duration::from_secs(3 * 24 * 3600);
        for cross in [false, true] {
            File::options()
                .write(true)
                .open(&from)
                .unwrap()
                .set_modified(old)
                .unwrap();
            if cross {
                test_hook::cross_device(|| stage(&from, &tmp)).unwrap();
            } else {
                stage(&from, &tmp).unwrap();
            }
            let age = SystemTime::now()
                .duration_since(fs::metadata(&tmp).unwrap().modified().unwrap())
                .unwrap_or_default();
            assert!(age < Duration::from_secs(3600), "cross {cross}: {age:?}");
            fs::remove_file(&tmp).unwrap();
        }
    }

    #[test]
    fn cross_device_staging_copies_and_keeps_the_source() {
        let (_d, from, dst, tmp) = dirs();
        let staged = test_hook::cross_device(|| stage(&from, &tmp)).unwrap();
        assert_eq!(staged, Staged::Copied);
        assert_eq!(std::fs::read(&tmp).unwrap(), b"payload");
        assert_eq!(std::fs::read(&from).unwrap(), b"payload");
        assert_eq!(names(&dst), [".session.tag.asteria-tmp"]);
    }

    /// A taken temp name is found before anything is copied, and left alone.
    #[test]
    fn staging_never_clobbers() {
        let (_d, from, _dst, tmp) = dirs();
        for cross in [false, true] {
            std::fs::write(&tmp, b"theirs").unwrap();
            let r = if cross {
                test_hook::cross_device(|| stage(&from, &tmp))
            } else {
                stage(&from, &tmp)
            };
            assert_eq!(r.unwrap_err().kind(), ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(&tmp).unwrap(), b"theirs");
        }
    }

    #[test]
    fn staging_a_missing_source_fails_without_litter() {
        let (_d, from, dst, tmp) = dirs();
        std::fs::remove_file(&from).unwrap();
        assert!(stage(&from, &tmp).is_err());
        assert!(test_hook::cross_device(|| stage(&from, &tmp)).is_err());
        assert!(names(&dst).is_empty());
    }

    /// The copy fails after its temp file exists: the temp file is removed.
    #[cfg(unix)]
    #[test]
    fn a_copy_failing_midway_leaves_no_temp_file() {
        let (_d, from, dst, tmp) = dirs();
        // A directory opens on unix, but reading it fails.
        let from = from.parent().unwrap();
        assert!(test_hook::cross_device(|| stage(from, &tmp)).is_err());
        assert!(names(&dst).is_empty());
    }

    #[test]
    fn cross_device_errors_are_recognised() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let exdev = 18;
        #[cfg(windows)]
        let exdev = 17; // ERROR_NOT_SAME_DEVICE
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        assert!(is_cross_device(&io::Error::from_raw_os_error(exdev)));
        assert!(is_cross_device(&ErrorKind::CrossesDevices.into()));
        assert!(!is_cross_device(&ErrorKind::PermissionDenied.into()));
        assert!(!is_cross_device(&ErrorKind::AlreadyExists.into()));
    }

    #[test]
    fn the_cross_device_hook_is_reset_after_a_panic() {
        let r = std::panic::catch_unwind(|| test_hook::cross_device(|| panic!("in the hook")));
        assert!(r.is_err());
        assert!(!test_hook::is_cross_device());
    }

    #[test]
    fn fallback_decision() {
        use std::io::{Error, ErrorKind::*};
        assert!(!should_fallback(&Error::from(AlreadyExists)));
        assert!(should_fallback(&Error::from(PermissionDenied)));
        assert!(should_fallback(&Error::from(Unsupported)));
    }
}
