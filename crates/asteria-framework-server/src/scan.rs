//! Incremental library scanner: walks the library, diffs against the cache by
//! (size, mtime), refreshes sidecar snapshots and removes vanished items.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use asteria_framework::paths::is_sidecar;
use asteria_framework::{Error as FrameworkError, Library, classify};
use walkdir::WalkDir;

use crate::cache::{Cache, ExtraRecord, ItemRecord, SidecarSnapshot, item_id_for};
use crate::error::ServerError;
use crate::ext::AppExtension;

const BATCH: usize = 512;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    /// `(path, reason)` of entries that could not be scanned.
    pub skipped: Vec<(String, String)>,
}

pub(crate) fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    let Ok(t) = meta.modified() else { return 0 };
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

pub(crate) fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

/// Library-relative `/`-joined path, or `None` if any component is not UTF-8.
pub(crate) fn rel_string(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// Library-relative `/`-joined path for reporting; non-UTF-8 parts are replaced lossily.
pub(crate) fn rel_lossy(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    parts.join("/")
}

/// A symlink target must lie inside the library and obey the same path policy as
/// ordinary entries: no component starting with `.` (covers `.asteria`), not a sidecar.
fn target_allowed(canonical_root: &Path, target: &Path) -> bool {
    let Ok(rel) = target.strip_prefix(canonical_root) else {
        return false;
    };
    !rel.components().any(|c| is_hidden(c.as_os_str())) && !is_sidecar(target)
}

/// Scans the library into `cache`. Does not clean leftover tmp files: that is done
/// once at server startup, because running it here would race in-flight atomic writes.
pub fn scan(
    library: &Library,
    cache: &Cache,
    ext: &dyn AppExtension,
) -> Result<ScanReport, ServerError> {
    scan_cancellable(library, cache, ext, &AtomicBool::new(false))
}

/// [`scan`] that stops between entries once `cancel` is set. A cancelled scan keeps what
/// it has written but skips the removal pass: unvisited paths are not known to be gone.
pub fn scan_cancellable(
    library: &Library,
    cache: &Cache,
    ext: &dyn AppExtension,
    cancel: &AtomicBool,
) -> Result<ScanReport, ServerError> {
    let root = library.root();
    // Failing to read the root aborts the scan.
    std::fs::read_dir(root)?;
    check_identity(library)?;
    let canonical_root = root.canonicalize()?;
    let lib_id = library.config().id;

    let mut report = ScanReport::default();
    let mut seen: HashSet<String> = HashSet::new();
    // Subtrees we could not read: their cached records must not be treated as vanished.
    let mut unreadable: Vec<String> = Vec::new();
    let mut pending: Vec<ItemRecord> = Vec::new();
    let mut pending_app: Vec<(String, Option<Vec<u8>>)> = Vec::new();
    let known_extras: HashMap<String, ExtraRecord> = cache.extras()?.into_iter().collect();
    let mut seen_extras: HashSet<String> = HashSet::new();
    let mut pending_extras: Vec<(String, ExtraRecord)> = Vec::new();

    let mut it = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter();
    let mut cancelled = false;
    while let Some(next) = it.next() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let entry = match next {
            Ok(e) => e,
            Err(e) => {
                if e.depth() == 0 {
                    return Err(ServerError::Io(e.into()));
                }
                let path = e.path().map(|p| p.to_path_buf()).unwrap_or_default();
                let rel = rel_lossy(root, &path);
                tracing::warn!(path = %rel, error = %e, "unreadable subtree; keeping its cached records");
                unreadable.push(rel.clone());
                report.skipped.push((rel, e.to_string()));
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        if is_hidden(entry.file_name()) {
            if entry.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        let ft = entry.file_type();
        let Some(rel) = rel_string(root, entry.path()) else {
            if ft.is_dir() {
                it.skip_current_dir();
            }
            let lossy = rel_lossy(root, entry.path());
            tracing::warn!(path = %lossy, "skipping name that is not valid UTF-8");
            report
                .skipped
                .push((lossy, "name is not valid UTF-8".into()));
            continue;
        };
        if ft.is_dir() {
            continue;
        }
        if is_sidecar(entry.path()) {
            continue;
        }
        if ext.wants_extra(&rel) {
            if !ft.is_file() {
                continue;
            }
            // Keep a file we cannot stat out of the removal pass.
            seen_extras.insert(rel.clone());
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    report.skipped.push((rel, e.to_string()));
                    continue;
                }
            };
            let rec = ExtraRecord {
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
            };
            if known_extras.get(&rel) != Some(&rec) {
                pending_extras.push((rel, rec));
            }
            continue;
        }
        let meta = if ft.is_symlink() {
            // Only links that resolve to a file inside the library are accepted.
            match entry.path().canonicalize() {
                Ok(target) if target_allowed(&canonical_root, &target) => {}
                Ok(_) => continue,
                Err(e) => {
                    report.skipped.push((rel, e.to_string()));
                    continue;
                }
            }
            std::fs::metadata(entry.path())
        } else if ft.is_file() {
            entry.metadata().map_err(std::io::Error::from)
        } else {
            continue;
        };
        let meta = match meta {
            Ok(m) if m.is_file() => m,
            Ok(_) => continue,
            Err(e) => {
                report.skipped.push((rel, e.to_string()));
                continue;
            }
        };
        let size = meta.len();
        let mtime = mtime_ns(&meta);
        let existing = cache.get_by_path(&rel)?;
        // Only new or changed files are opened for classification.
        let unchanged = existing
            .as_ref()
            .filter(|old| old.size == size && old.mtime_ns == mtime);
        let kind = match unchanged {
            Some(old) => old.kind,
            None => match classify(entry.path()) {
                Ok(k) => k,
                Err(e) => {
                    report.skipped.push((rel, e.to_string()));
                    continue;
                }
            },
        };
        if !ext.accepts(kind) {
            continue;
        }
        let hash = existing
            .as_ref()
            .filter(|old| old.size == size && old.mtime_ns == mtime)
            .and_then(|old| old.hash.clone())
            .filter(|h| h.size == size && h.mtime_ns == mtime);
        let (sidecar, sidecar_error) = match library.read_sidecar(Path::new(&rel)) {
            Ok(Some(sc)) => (SidecarSnapshot::from(&sc), None),
            Ok(None) => (SidecarSnapshot::default(), None),
            Err(FrameworkError::MalformedSidecar { reason, .. }) => {
                // Only the reason: the Display form carries the absolute sidecar path.
                (SidecarSnapshot::default(), Some(reason))
            }
            Err(e) => {
                // Keep the path out of the removal pass: we could not look at it.
                seen.insert(rel.clone());
                report.skipped.push((rel, e.to_string()));
                continue;
            }
        };
        // Only a new or (size, mtime)-changed file is extracted; a sidecar edit is not.
        let extracted = unchanged
            .is_none()
            .then(|| extract_guarded(ext, &rel, entry.path()));
        let rec = ItemRecord {
            id: item_id_for(&lib_id, &rel),
            path: rel.clone(),
            kind,
            size,
            mtime_ns: mtime,
            hash,
            sidecar,
            sidecar_error,
        };
        seen.insert(rel);
        match existing {
            None => report.added += 1,
            Some(old) if old != rec => report.updated += 1,
            Some(_) => continue,
        }
        if let Some(data) = extracted {
            pending_app.push((rec.path.clone(), data));
        }
        pending.push(rec);
        if pending.len() >= BATCH {
            cache.put_many_with_app_data(&pending, &pending_app)?;
            pending.clear();
            pending_app.clear();
        }
    }
    if !pending.is_empty() {
        cache.put_many_with_app_data(&pending, &pending_app)?;
    }
    cache.put_extras(&pending_extras)?;
    if cancelled {
        tracing::info!("scan cancelled; skipping the removal pass");
        cache.flush()?;
        return Ok(report);
    }
    let gone_extras: Vec<String> = known_extras
        .into_keys()
        .filter(|p| !seen_extras.contains(p) && !under_any(p, &unreadable))
        .collect();
    cache.remove_extras(&gone_extras)?;

    let mut gone: Vec<String> = cache
        .all_paths()?
        .into_iter()
        .filter(|p| !seen.contains(p) && !under_any(p, &unreadable))
        .collect();
    // Files that vanished mid-scan were skipped by path; they are gone too.
    gone.sort();
    for chunk in gone.chunks(BATCH) {
        cache.remove_many(chunk)?;
    }
    report.removed = gone.len();
    cache.flush()?;
    Ok(report)
}

/// A panicking extension must not wedge every later scan at the same file: the panic
/// counts as "no app data" and the scan goes on.
pub(crate) fn extract_guarded(ext: &dyn AppExtension, rel: &str, abs: &Path) -> Option<Vec<u8>> {
    catch_unwind(AssertUnwindSafe(|| ext.extract(rel, abs))).unwrap_or_else(|_| {
        tracing::warn!(path = %rel, "app extension panicked while extracting; storing no app data");
        None
    })
}

/// The library's `.asteria/library.toml` must still be there and name the same library.
/// Otherwise (an unmounted disk, another library in its place) the scan would see every
/// item as vanished, so it stops before removing anything.
fn check_identity(library: &Library) -> Result<(), ServerError> {
    let on_disk = Library::open(library.root()).map_err(|e| {
        tracing::error!(error = %e, "library metadata is missing or unreadable; not scanning");
        ServerError::LibraryUnavailable("library metadata is missing or unreadable".into())
    })?;
    if on_disk.config().id != library.config().id {
        tracing::error!("the library directory now holds another library; not scanning");
        return Err(ServerError::LibraryUnavailable(
            "the directory holds another library".into(),
        ));
    }
    Ok(())
}

fn under_any(path: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|d| {
        path == d
            || path
                .strip_prefix(d.as_str())
                .is_some_and(|r| r.starts_with('/'))
    })
}
