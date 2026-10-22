//! Background payload hashing of cached items and re-linking of orphaned XMP sidecars.
//!
//! Both functions are synchronous. `relink_orphans` writes to the library (it moves a
//! sidecar), so in production it must be called from the library's write queue.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use asteria_framework::hash::content_hash;
use asteria_framework::journal::{Actor, Journal, JournalEntry, OpId, OpKind};
use asteria_framework::paths::{is_sidecar, media_path_for_sidecar, sidecar_path_for};
use asteria_framework::{Library, Sidecar};
use chrono::Utc;
use grain_id::GrainId;
use walkdir::WalkDir;

use crate::cache::{Cache, ItemRecord, SidecarSnapshot, StoredHash};
use crate::error::ServerError;
use crate::scan::{is_hidden, mtime_ns, rel_lossy, rel_string};

const BATCH: usize = 64;

/// Computes the content hash of every cached item whose stored hash is missing or no
/// longer matches the file's (size, mtime). Returns how many hashes were stored.
/// Stops early (keeping what is done) once `cancel` is set. Per-file errors are logged
/// and skipped; cache errors propagate.
pub fn hash_pending(
    library: &Library,
    cache: &Cache,
    cancel: &AtomicBool,
) -> Result<usize, ServerError> {
    let mut done = 0;
    let mut pending: Vec<ItemRecord> = Vec::new();
    for path in cache.all_paths()? {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(mut rec) = cache.get_by_path(&path)? else {
            continue;
        };
        let abs = library.root().join(&path);
        let Ok(before) = std::fs::metadata(&abs) else {
            tracing::debug!(%path, "file vanished before hashing");
            continue;
        };
        let (size, mtime) = (before.len(), mtime_ns(&before));
        if rec
            .hash
            .as_ref()
            .is_some_and(|h| h.size == size && h.mtime_ns == mtime)
        {
            continue;
        }
        let outcome = match content_hash(&abs) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(%path, error = %e, "hashing failed; skipping");
                continue;
            }
        };
        // The file may have changed (or vanished) while we read it: leave it for next run.
        match std::fs::metadata(&abs) {
            Ok(after) if after.len() == size && mtime_ns(&after) == mtime => {}
            _ => {
                tracing::debug!(%path, "file changed while hashing; not storing hash");
                continue;
            }
        }
        rec.hash = Some(StoredHash {
            hash: outcome.hash.to_string(),
            scope: outcome.scope.to_string(),
            size,
            mtime_ns: mtime,
        });
        pending.push(rec);
        done += 1;
        if pending.len() >= BATCH {
            cache.put_many(&pending)?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        cache.put_many(&pending)?;
    }
    cache.flush()?;
    Ok(done)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RelinkReport {
    /// `(old sidecar path, new media path)`.
    pub relinked: Vec<(String, String)>,
    /// Orphan sidecars whose hash matches two or more cached items (left in place).
    pub ambiguous: Vec<String>,
    /// Orphan sidecars with no usable match (left in place).
    pub unmatched: Vec<String>,
}

/// Moves each orphaned sidecar (its media file is gone) next to the one cached item with
/// the same content hash and scope, provided that item has no sidecar on disk. The move
/// is journaled and the item's cached sidecar snapshot refreshed.
pub fn relink_orphans(library: &Library, cache: &Cache) -> Result<RelinkReport, ServerError> {
    let root = library.root();
    let mut orphans: Vec<String> = Vec::new();
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
                tracing::warn!(error = %e, "unreadable entry while looking for orphan sidecars");
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
        if !entry.file_type().is_file() || !is_sidecar(entry.path()) {
            continue;
        }
        let Some(rel) = rel_string(root, entry.path()) else {
            tracing::warn!(path = %rel_lossy(root, entry.path()), "skipping sidecar whose name is not valid UTF-8");
            continue;
        };
        if let Some(media) = media_path_for_sidecar(entry.path())
            && media.symlink_metadata().is_err()
        {
            orphans.push(rel);
        }
    }

    let mut report = RelinkReport::default();
    if orphans.is_empty() {
        return Ok(report);
    }

    // (hash, scope) -> media paths, from records whose stored hash is still current.
    let mut index: std::collections::HashMap<(String, String), Vec<String>> = Default::default();
    for path in cache.all_paths()? {
        let Some(rec) = cache.get_by_path(&path)? else {
            continue;
        };
        if let Some(h) = rec.hash
            && h.size == rec.size
            && h.mtime_ns == rec.mtime_ns
        {
            index.entry((h.hash, h.scope)).or_default().push(path);
        }
    }

    let journal = Journal::new(library);
    for old in orphans {
        let sc = match Sidecar::read(&root.join(&old)) {
            Ok(Some(sc)) => sc,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(sidecar = %old, error = %e, "unreadable orphan sidecar; not moving it");
                report.unmatched.push(old);
                continue;
            }
        };
        let Some((hash, scope)) = sc.content_hash else {
            report.unmatched.push(old);
            continue;
        };
        let candidates = index
            .get(&(hash.to_string(), scope.to_string()))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let target = match candidates {
            [] => {
                report.unmatched.push(old);
                continue;
            }
            [one] => one.clone(),
            _ => {
                report.ambiguous.push(old);
                continue;
            }
        };
        let new_sidecar_abs = sidecar_path_for(&root.join(&target));
        let new_sidecar = rel_lossy(root, &new_sidecar_abs);
        // Check the filesystem, not the cached snapshot: the sidecar may be newer than the scan.
        if new_sidecar_abs.symlink_metadata().is_ok() {
            tracing::warn!(sidecar = %old, %target, "target already has a sidecar; not relinking");
            report.unmatched.push(old);
            continue;
        }
        if let Err(e) = move_no_clobber(&root.join(&old), &new_sidecar_abs) {
            tracing::warn!(sidecar = %old, %target, error = %e, "could not move sidecar");
            report.unmatched.push(old);
            continue;
        }
        journal.append(&JournalEntry {
            op_id: OpId(GrainId::random()),
            plan_id: None,
            actor: Actor::User(library.owner()),
            at: Utc::now(),
            kind: OpKind::RelinkSidecar,
            before: serde_json::json!({ "path": old }),
            after: serde_json::json!({ "path": new_sidecar }),
        })?;
        refresh_snapshot(library, cache, &target)?;
        // Another orphan with the same hash must not claim this target again; the
        // filesystem check above also guards it, this just keeps the report accurate.
        report.relinked.push((old, target));
    }
    cache.flush()?;
    Ok(report)
}

fn refresh_snapshot(library: &Library, cache: &Cache, media: &str) -> Result<(), ServerError> {
    let Some(mut rec) = cache.get_by_path(media)? else {
        return Ok(());
    };
    match library.read_sidecar(Path::new(media)) {
        Ok(Some(sc)) => {
            rec.sidecar = SidecarSnapshot::from(&sc);
            rec.sidecar_error = None;
        }
        Ok(None) => {
            rec.sidecar = SidecarSnapshot::default();
            rec.sidecar_error = None;
        }
        Err(e) => {
            rec.sidecar = SidecarSnapshot::default();
            rec.sidecar_error = Some(e.to_string());
        }
    }
    cache.put(&rec)
}

/// Moves `from` to `to` without ever replacing an existing `to`.
///
/// On unix this is `hard_link` (which fails if `to` exists) followed by removing `from`,
/// so a racing creator of `to` is never overwritten. If the filesystem cannot hard-link,
/// it falls back to check-then-`rename`, which is acceptable because all library writes go
/// through the single write queue.
fn move_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            return std::fs::remove_file(from).inspect_err(|_| {
                let _ = std::fs::remove_file(to);
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {}
    }
    if to.symlink_metadata().is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}
