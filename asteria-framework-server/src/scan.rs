//! Incremental library scanner: walks the library, diffs against the cache by
//! (size, mtime), refreshes sidecar snapshots and removes vanished items.

use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

use asteria_framework::paths::is_sidecar;
use asteria_framework::{Error as FrameworkError, Library, MediaKind, classify, fsutil};
use walkdir::WalkDir;

use crate::cache::{Cache, ItemRecord, SidecarSnapshot, item_id_for};
use crate::error::ServerError;

const BATCH: usize = 512;

/// Decides which kinds of media an app turns into items.
pub trait MediaFilter: Send + Sync + 'static {
    fn accepts(&self, kind: MediaKind) -> bool;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    /// `(path, reason)` of entries that could not be scanned.
    pub skipped: Vec<(String, String)>,
}

fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    let Ok(t) = meta.modified() else { return 0 };
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

/// Library-relative `/`-joined path, or `None` if any component is not UTF-8.
fn rel_string(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

pub fn scan(
    library: &Library,
    cache: &Cache,
    filter: &dyn MediaFilter,
) -> Result<ScanReport, ServerError> {
    let root = library.root();
    fsutil::cleanup_tmp_files(root)?;
    // Failing to read the root aborts the scan.
    std::fs::read_dir(root)?;
    let canonical_root = root.canonicalize()?;
    let lib_id = library.config().id;

    let mut report = ScanReport::default();
    let mut seen: HashSet<String> = HashSet::new();
    // Subtrees we could not read: their cached records must not be treated as vanished.
    let mut unreadable: Vec<String> = Vec::new();
    let mut pending: Vec<ItemRecord> = Vec::new();

    let mut it = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter();
    while let Some(next) = it.next() {
        let entry = match next {
            Ok(e) => e,
            Err(e) => {
                if e.depth() == 0 {
                    return Err(ServerError::Io(e.into()));
                }
                let path = e.path().map(|p| p.to_path_buf()).unwrap_or_default();
                let rel =
                    rel_string(root, &path).unwrap_or_else(|| path.to_string_lossy().into_owned());
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
            report.skipped.push((
                entry.path().to_string_lossy().into_owned(),
                "name is not valid UTF-8".into(),
            ));
            continue;
        };
        if ft.is_dir() {
            continue;
        }
        if is_sidecar(entry.path()) {
            continue;
        }
        let meta = if ft.is_symlink() {
            // Only links that resolve to a file inside the library are accepted.
            match entry.path().canonicalize() {
                Ok(target) if target.starts_with(&canonical_root) => {}
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
        let kind = match classify(entry.path()) {
            Ok(k) => k,
            Err(e) => {
                report.skipped.push((rel, e.to_string()));
                continue;
            }
        };
        if !filter.accepts(kind) {
            continue;
        }
        let size = meta.len();
        let mtime = mtime_ns(&meta);
        let existing = cache.get_by_path(&rel)?;
        let hash = existing
            .as_ref()
            .filter(|old| old.size == size && old.mtime_ns == mtime)
            .and_then(|old| old.hash.clone())
            .filter(|h| h.size == size && h.mtime_ns == mtime);
        let (sidecar, sidecar_error) = match library.read_sidecar(Path::new(&rel)) {
            Ok(Some(sc)) => (SidecarSnapshot::from(&sc), None),
            Ok(None) => (SidecarSnapshot::default(), None),
            Err(e @ FrameworkError::MalformedSidecar { .. }) => {
                (SidecarSnapshot::default(), Some(e.to_string()))
            }
            Err(e) => {
                // Keep the path out of the removal pass: we could not look at it.
                seen.insert(rel.clone());
                report.skipped.push((rel, e.to_string()));
                continue;
            }
        };
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
        pending.push(rec);
        if pending.len() >= BATCH {
            cache.put_many(&pending)?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        cache.put_many(&pending)?;
    }

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

fn under_any(path: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|d| {
        path == d
            || path
                .strip_prefix(d.as_str())
                .is_some_and(|r| r.starts_with('/'))
    })
}
