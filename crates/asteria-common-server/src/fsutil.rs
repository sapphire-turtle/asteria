//! Filesystem helpers shared by the writers of original files.

use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use corundum_framework::platform::fs::rename_no_replace;

/// Whether a failed `hard_link` should be retried as check-then-rename: every error except
/// `AlreadyExists`. Some filesystems (vfat, exFAT) report EPERM for hard links; if the
/// problem is a real permission error the rename fails too and that error is reported.
fn should_fallback(e: &std::io::Error) -> bool {
    e.kind() != ErrorKind::AlreadyExists
}

/// Moves `from` to `to` without ever replacing an existing `to`.
///
/// One atomic rename that refuses to replace (corundum's `rename_no_replace`: `renameat2`,
/// `renameatx_np` or `MoveFileExW`), so that a crash never leaves the file under both names
/// (#36). Where the OS or the filesystem has none (`Unsupported`), it falls back on
/// `hard_link` (fails if `to` exists, so a racing creator is never overwritten) followed by
/// removing `from`; a crash between the two leaves both names. If `hard_link` fails for any
/// other reason it falls back to check-then-`rename`, which is acceptable because all library
/// writes go through the single write queue.
pub(crate) fn move_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    match rename_no_replace(from, to) {
        Err(e) if e.kind() == ErrorKind::Unsupported => {
            tracing::debug!(path = %to.display(), error = %e, "no atomic rename; linking instead");
        }
        r => return r,
    }
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

/// Why [`create_dirs_inside`] did not make a folder.
#[derive(Debug)]
pub(crate) enum DirError {
    /// The folder (library-relative, `/`-separated) is a symbolic link or a junction; going
    /// through it could leave the library (#32).
    Linked(String),
    Io(io::Error),
}

/// Checks the folders of `rel` (library-relative, `/`-separated) that already exist below
/// `root`, from the top down to the first missing one: each must be a real folder, not a
/// symbolic link or a junction. `root` itself may be a link.
pub(crate) fn check_dirs_inside(root: &Path, rel: &str) -> Result<(), DirError> {
    let mut dir = root.to_path_buf();
    for (i, part) in components(rel) {
        dir.push(part);
        match real_dir(&dir, &rel[..i]) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Makes the folder `rel` (library-relative, `/`-separated) below `root` one level at a
/// time, never through a symbolic link or a junction ([`check_dirs_inside`]), unlike
/// `create_dir_all`. Returns the topmost folder it made, if any. On failure the folders it
/// made are removed again.
pub(crate) fn create_dirs_inside(root: &Path, rel: &str) -> Result<Option<PathBuf>, DirError> {
    let mut dir = root.to_path_buf();
    let mut top: Option<PathBuf> = None;
    let undo = |top: &Option<PathBuf>, dir: &Path| {
        if let Some(top) = top {
            for d in dir.ancestors() {
                let _ = fs::remove_dir(d);
                if d == top {
                    break;
                }
            }
        }
    };
    for (i, part) in components(rel) {
        dir.push(part);
        let made = match real_dir(&dir, &rel[..i]) {
            Ok(true) => continue,
            Ok(false) => fs::create_dir(&dir),
            Err(e) => {
                dir.pop();
                undo(&top, &dir);
                return Err(e);
            }
        };
        match made {
            Ok(()) => {
                top.get_or_insert_with(|| dir.clone());
            }
            // Made meanwhile by someone else: it must be a real folder, too.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if let Err(e) = real_dir(&dir, &rel[..i])
                    .and_then(|real| if real { Ok(()) } else { Err(DirError::Io(e)) })
                {
                    dir.pop();
                    undo(&top, &dir);
                    return Err(e);
                }
            }
            Err(e) => {
                dir.pop();
                undo(&top, &dir);
                return Err(DirError::Io(e));
            }
        }
    }
    Ok(top)
}

/// The non-empty components of `rel`, each with the length of `rel` up to its end.
fn components(rel: &str) -> impl Iterator<Item = (usize, &str)> {
    rel.split('/')
        .scan(0, |end, part| {
            let start = *end;
            *end = start + part.len() + 1;
            Some((start + part.len(), part))
        })
        .filter(|(_, part)| !part.is_empty())
}

/// Whether `dir` is a real folder (`false`: it does not exist); a link is
/// [`DirError::Linked`], named by `rel`.
fn real_dir(dir: &Path, rel: &str) -> Result<bool, DirError> {
    match dir.symlink_metadata() {
        // Junctions are links too (std counts every name-surrogate reparse point).
        Ok(m) if m.file_type().is_symlink() => Err(DirError::Linked(rel.to_string())),
        Ok(m) if m.is_dir() => Ok(true),
        Ok(_) => Err(DirError::Io(ErrorKind::NotADirectory.into())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(DirError::Io(e)),
    }
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
    fn folders_are_made_one_level_at_a_time() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("lib");
        std::fs::create_dir_all(root.join("Archive")).unwrap();
        let top = create_dirs_inside(&root, "Archive/2026/05").unwrap();
        assert_eq!(top, Some(root.join("Archive/2026")));
        assert!(root.join("Archive/2026/05").is_dir());
        assert_eq!(create_dirs_inside(&root, "Archive/2026/05").unwrap(), None);
        assert_eq!(create_dirs_inside(&root, "").unwrap(), None);
        assert!(check_dirs_inside(&root, "Archive/2026/05/x/y").is_ok());
    }

    /// A link anywhere below the root stops, before anything is made; the root may be one.
    #[cfg(unix)]
    #[test]
    fn a_linked_folder_below_the_root_stops() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let (real, outside) = (d.path().join("real"), d.path().join("outside"));
        std::fs::create_dir_all(real.join("Archive")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root = d.path().join("lib");
        symlink(&real, &root).unwrap();
        symlink(&outside, real.join("Archive/2026")).unwrap();
        symlink(&outside, real.join("Out")).unwrap();

        for (rel, linked) in [("Out/x", "Out"), ("Archive/2026/05", "Archive/2026")] {
            match check_dirs_inside(&root, rel) {
                Err(DirError::Linked(l)) => assert_eq!(l, linked),
                r => panic!("{rel}: {r:?}"),
            }
            match create_dirs_inside(&root, rel) {
                Err(DirError::Linked(l)) => assert_eq!(l, linked),
                r => panic!("{rel}: {r:?}"),
            }
        }
        assert!(names(&outside).is_empty());
        // Through the linked root itself is fine.
        assert_eq!(
            create_dirs_inside(&root, "Archive/2025").unwrap(),
            Some(root.join("Archive/2025"))
        );
        assert!(real.join("Archive/2025").is_dir());
    }

    #[test]
    fn a_file_in_the_way_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f"), b"").unwrap();
        assert!(matches!(
            create_dirs_inside(d.path(), "f/x"),
            Err(DirError::Io(_))
        ));
    }

    #[test]
    fn a_move_never_replaces() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let e = move_no_clobber(&a, &b).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::AlreadyExists);
        assert_eq!(
            (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap()),
            (b"a".to_vec(), b"b".to_vec())
        );
        move_no_clobber(&a, &c).unwrap();
        assert_eq!(names(d.path()), ["b", "c"]);
        assert_eq!(std::fs::read(&c).unwrap(), b"a");
    }

    #[test]
    fn fallback_decision() {
        use std::io::{Error, ErrorKind::*};
        assert!(!should_fallback(&Error::from(AlreadyExists)));
        assert!(should_fallback(&Error::from(PermissionDenied)));
        assert!(should_fallback(&Error::from(Unsupported)));
    }
}
