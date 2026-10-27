//! File helpers shared by the stores that keep a file in the app's data or config directory.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where an unreadable `path` is moved: `<name>.bad-<unix time>`, with `-<n>` added if taken.
pub(crate) fn bad_path(path: &Path) -> PathBuf {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".bad-{secs}"));
    let mut candidate = path.with_file_name(&name);
    let mut n = 1;
    while candidate.exists() {
        let mut alt = name.clone();
        alt.push(format!("-{n}"));
        candidate = path.with_file_name(alt);
        n += 1;
    }
    candidate
}

/// Writes a temp file in the same directory (mode 0o600 from the start on Unix when `private`),
/// then renames it over `path`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".asteria-tmp");
    let tmp = path.with_file_name(tmp_name);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // A leftover from a crashed save is ours to replace.
    let _ = fs::remove_file(&tmp);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        // Created private, so there is no moment with wider permissions.
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut f = opts.open(&tmp)?;
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
