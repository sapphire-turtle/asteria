//! Crash-safe file writes: write a sibling temp file, fsync, rename over the target.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};

const TMP_SUFFIX: &str = ".asteria-tmp";
/// How old a temp file must be before [`cleanup_tmp_files`] takes it for a leftover: no write
/// takes this long, so an older one has no live writer, in this instance or another.
const TMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// A random tag for this process, so that its temp names differ from those of other instances
/// sharing the workspace and of earlier runs.
fn instance_tag() -> &'static str {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| uuid::Uuid::new_v4().simple().to_string()[..8].to_owned())
}

/// The temp file `atomic_write` uses for `path`: `.<file name>.<tag>.asteria-tmp` in the same
/// directory, where `<tag>` is this process's own (see [`instance_tag`]).
pub fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".");
    name.push(instance_tag());
    name.push(TMP_SUFFIX);
    path.with_file_name(name)
}

/// Replace `path` with `bytes` so readers see either the old or the new content, never a mix.
///
/// The temp file is created exclusively; if it already exists (a concurrent writer in this
/// process) this fails with [`Error::TmpExists`] and leaves it alone. Temp files of other
/// processes, including a crashed earlier run, have another tag and do not get in the way. The
/// target's permission bits are kept, and a symlink target is replaced at its resolved path.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.file_name().is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path has no file name").into());
    }
    let path = resolve_link(path);
    let tmp = tmp_path_for(&path);
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(Error::TmpExists(tmp)),
        Err(e) => return Err(e.into()),
    };
    let result = write_and_rename(&mut f, &tmp, &path, bytes);
    drop(f);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(result?)
}

/// Follow symlinks (even dangling ones) so the link itself survives the rename.
fn resolve_link(path: &Path) -> PathBuf {
    let mut cur = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => match fs::read_link(&cur) {
                Ok(t) => cur = cur.parent().map_or(t.clone(), |p| p.join(&t)),
                Err(_) => return cur,
            },
            _ => return cur,
        }
    }
    cur
}

fn write_and_rename(f: &mut fs::File, tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Ok(meta) = fs::metadata(path) {
        f.set_permissions(meta.permissions())?;
    }
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(tmp, path)?;
    // Persist the rename itself; not possible on every platform, so best effort.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Delete leftover `.*.asteria-tmp` files under `root` (symlinks are not followed) that were
/// last modified more than 24 hours ago; a newer one may belong to a live writer, possibly
/// another instance sharing the workspace. Returns how many were removed. Unreadable
/// subdirectories and undeletable files are skipped with a warning; only an unreadable `root`
/// is an error.
pub fn cleanup_tmp_files(root: &Path) -> Result<usize> {
    let cutoff = SystemTime::now()
        .checked_sub(TMP_MAX_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    cleanup_tmp_files_before(root, cutoff)
}

fn cleanup_tmp_files_before(root: &Path, cutoff: SystemTime) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            match cleanup_tmp_files_before(&entry.path(), cutoff) {
                Ok(n) => removed += n,
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), error = %e, "skipping unreadable directory")
                }
            }
        } else if kind.is_file()
            && is_tmp_name(&entry.file_name().to_string_lossy())
            && is_older(&entry, cutoff)
        {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), error = %e, "cannot remove tmp file")
                }
            }
        }
    }
    Ok(removed)
}

/// Whether `entry` was last modified before `cutoff`; an unreadable time counts as new.
fn is_older(entry: &fs::DirEntry, cutoff: SystemTime) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|t| t < cutoff)
}

/// Both the tagged form `.<name>.<tag>.asteria-tmp` and the untagged `.<name>.asteria-tmp`
/// that earlier versions wrote.
fn is_tmp_name(name: &str) -> bool {
    name.len() > 1 + TMP_SUFFIX.len() && name.starts_with('.') && name.ends_with(TMP_SUFFIX)
}
