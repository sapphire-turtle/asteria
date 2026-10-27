//! A temporary cache of thumbnails and previews on disk, shared by every profile that points
//! at the same library.
//!
//! Files live at `<cache root>/<library id>/temp/<preset>/<hex[0..2]>/<hex>.webp`, where `hex`
//! is the item's `file_hash` without its `blake3:` prefix. The key is the content hash and the
//! preset only; an item whose hash is not known yet (`None`) is never stored.
//!
//! The library id and the preset become path components, and the library id comes from the
//! server, so both are checked (see [`MediaCache::open`]); a hash must be `blake3:` plus 64
//! lowercase hex digits. Nothing a server says can place a file outside the cache.
//!
//! Every method does blocking disk I/O: call them from a worker thread, never the UI thread.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

/// The size the cache is trimmed to unless told otherwise. Fixed for now; a setting comes later.
pub const DEFAULT_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

/// [`MediaCache::put`] trims the cache on every this-many-th write.
pub const TRIM_EVERY_PUTS: usize = 100;

const HASH_PREFIX: &str = "blake3:";
const TMP_SUFFIX: &str = ".asteria-tmp";
/// A temporary file this old is the leftover of a crashed write, not one in progress.
const STALE_TMP: Duration = Duration::from_secs(3600);

/// Distinguishes the temporary files of concurrent writers in one process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The cache of one library. Cheap to share behind an `Arc`; all methods take `&self`.
#[derive(Debug)]
pub struct MediaCache {
    /// `<cache root>/<library id>/temp`.
    root: PathBuf,
    /// [`trim`](Self::trim) deletes the least recently used files until the total is at most this.
    pub limit_bytes: u64,
    puts: AtomicUsize,
    /// Set while a trim runs, so overlapping calls return at once.
    trimming: AtomicBool,
}

impl MediaCache {
    /// The cache for `library_id` under `cache_root`, limited to [`DEFAULT_LIMIT_BYTES`]. Does
    /// no I/O; directories are made on the first write. `None` if `library_id` is not a safe
    /// single path component on every platform (letters, digits, `.`, `_`, `-`; no leading or trailing dot; no Windows device names).
    pub fn open(cache_root: &Path, library_id: &str) -> Option<MediaCache> {
        if !safe_component(library_id) {
            return None;
        }
        Some(MediaCache {
            root: cache_root.join(library_id).join("temp"),
            limit_bytes: DEFAULT_LIMIT_BYTES,
            puts: AtomicUsize::new(0),
            trimming: AtomicBool::new(false),
        })
    }

    /// The cached file for `file_hash` at `preset`, if there is one; a hit counts as a use for
    /// [`trim`](Self::trim). `None` for a missing, unknown (`None`) or malformed hash or preset.
    /// A later trim may remove the returned file, so a caller that fails to open it treats that
    /// as a miss.
    pub fn get(&self, file_hash: Option<&str>, preset: &str) -> Option<PathBuf> {
        let path = self.path(file_hash?, preset).ok()?;
        // Opened for writing: Windows needs that to change the modification time.
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(file) => {
                if !file.metadata().ok()?.is_file() {
                    return None;
                }
                // Best effort: a failed touch does not turn a hit into a miss.
                let _ = file.set_modified(SystemTime::now());
            }
            // Read-only file or volume: still a hit if it can be read.
            Err(_) if fs::metadata(&path).is_ok_and(|m| m.is_file()) => {}
            Err(_) => return None,
        }
        Some(path)
    }

    /// Stores `bytes` for `file_hash` at `preset` and returns where. The file appears all at
    /// once (written next to its place, then renamed). `Ok(None)` if `file_hash` is `None`:
    /// nothing is stored. An error for a malformed hash or preset, or on I/O failure.
    /// Every [`TRIM_EVERY_PUTS`]-th write also runs [`trim`](Self::trim), on this thread.
    pub fn put(
        &self,
        file_hash: Option<&str>,
        preset: &str,
        bytes: &[u8],
    ) -> io::Result<Option<PathBuf>> {
        let Some(file_hash) = file_hash else {
            return Ok(None);
        };
        let path = self.path(file_hash, preset)?;
        write_atomic(&path, bytes)?;
        if (self.puts.fetch_add(1, Ordering::Relaxed) + 1).is_multiple_of(TRIM_EVERY_PUTS) {
            self.trim();
        }
        Ok(Some(path))
    }

    /// Deletes files, least recently used first (by modification time), until the total size is
    /// at most [`limit_bytes`](Self::limit_bytes). Temporary files of writes in progress are not
    /// counted or deleted; stale ones are removed. Failures are skipped.
    /// If another trim is already running, returns at once.
    pub fn trim(&self) {
        if self.trimming.swap(true, Ordering::Acquire) {
            return;
        }
        self.trim_files();
        self.trimming.store(false, Ordering::Release);
    }

    fn trim_files(&self) {
        let mut files = Vec::new();
        let mut total = 0u64;
        let now = SystemTime::now();
        for preset in subdirs(&self.root) {
            for bucket in subdirs(&preset) {
                let Ok(entries) = fs::read_dir(&bucket) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let Ok(meta) = entry.metadata() else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    let path = entry.path();
                    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with('.') {
                        if name.ends_with(TMP_SUFFIX)
                            && now
                                .duration_since(modified)
                                .is_ok_and(|age| age > STALE_TMP)
                        {
                            let _ = fs::remove_file(&path);
                        }
                        continue;
                    }
                    total += meta.len();
                    files.push((modified, meta.len(), path));
                }
            }
        }
        files.sort();
        for (_, len, path) in files {
            if total <= self.limit_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }

    fn path(&self, file_hash: &str, preset: &str) -> io::Result<PathBuf> {
        let invalid = |what| io::Error::new(io::ErrorKind::InvalidInput, what);
        if !safe_component(preset) {
            return Err(invalid("unsafe cache preset"));
        }
        let hex = file_hash
            .strip_prefix(HASH_PREFIX)
            .filter(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .ok_or_else(|| invalid("file hash is not blake3:<64 lowercase hex>"))?;
        Ok(self
            .root
            .join(preset)
            .join(&hex[..2])
            .join(format!("{hex}.webp")))
    }
}

/// A name that is one ordinary path component on every platform: only `[A-Za-z0-9._-]`, not
/// starting or ending with a dot (so no `.`, `..` or hidden names, which are also what
/// temporary files look like), and not a Windows reserved device name. This also rules out
/// separators, drive prefixes (`C:`), NUL, and trailing spaces.
pub(crate) fn safe_component(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or_default();
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

/// Writes `bytes` to a hidden file beside `path`, then renames it into place, so a reader never
/// sees half a file. (Unlike the profile store's writer there is no private mode and no fsync:
/// the cache holds nothing secret and a lost file is just fetched again.)
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no directory"))?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.{}-{}{TMP_SUFFIX}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
