//! Background payload hashing of cached items and re-linking of orphaned XMP sidecars.
//!
//! Both functions are synchronous. `relink_orphans` writes to the library (it moves a
//! sidecar), so in production it must be called from the library's write queue.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use asteria_framework::hash::content_hash;
use asteria_framework::journal::{Actor, Journal, JournalEntry, OpId, OpKind};
use asteria_framework::paths::{is_sidecar, media_path_for_sidecar, sidecar_path_for};
use asteria_framework::{Error as FwError, Library, Sidecar};
use chrono::Utc;
use grain_id::GrainId;
use walkdir::WalkDir;

use crate::cache::{Cache, SidecarSnapshot, StoredHash};
use crate::error::ServerError;
use crate::scan::{is_hidden, mtime_ns, rel_lossy, rel_string};

const BATCH: usize = 64;

/// Computes the content hash of every cached item whose stored hash is missing or no
/// longer matches the file's (size, mtime). Returns how many hashes were stored.
/// Stops early (keeping what is done) once `cancel` is set. Per-file errors are logged
/// and skipped; cache errors propagate. Only the `hash` field of a record is written, and
/// only if the record's size/mtime still match the hash. A record whose size/mtime no
/// longer match the file is skipped (debug log): run `scan` first to refresh it.
pub fn hash_pending(
    library: &Library,
    cache: &Cache,
    cancel: &AtomicBool,
) -> Result<usize, ServerError> {
    hash_pending_with(library, cache, cancel, &mut |_| {})
}

/// `hook` runs after a file was hashed and before it is re-stat'ed (test seam).
fn hash_pending_with(
    library: &Library,
    cache: &Cache,
    cancel: &AtomicBool,
    hook: &mut dyn FnMut(&str),
) -> Result<usize, ServerError> {
    let mut done = 0;
    let mut pending: Vec<(String, Option<StoredHash>)> = Vec::new();
    for path in cache.all_paths()? {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(rec) = cache.get_by_path(&path)? else {
            continue;
        };
        let abs = library.root().join(&path);
        let before = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    tracing::debug!(%path, "file vanished before hashing");
                } else {
                    tracing::warn!(%path, error = %e, "cannot stat file; skipping");
                }
                continue;
            }
        };
        let stamp = (before.len(), mtime_ns(&before));
        if stamp != (rec.size, rec.mtime_ns) {
            tracing::debug!(%path, "record is stale (file changed since scan); rescan first");
            continue;
        }
        if rec
            .hash
            .as_ref()
            .is_some_and(|h| (h.size, h.mtime_ns) == stamp)
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
        hook(&path);
        // The file may have changed (or vanished) while we read it: leave it for next run.
        if !unchanged_since(&abs, stamp) {
            tracing::debug!(%path, "file changed while hashing; not storing hash");
            continue;
        }
        pending.push((
            path,
            Some(StoredHash {
                hash: outcome.hash.to_string(),
                scope: outcome.scope.to_string(),
                size: stamp.0,
                mtime_ns: stamp.1,
            }),
        ));
        if pending.len() >= BATCH {
            done += cache.set_hash_many(&pending)?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        done += cache.set_hash_many(&pending)?;
    }
    cache.flush()?;
    Ok(done)
}

/// Whether the file at `path` still has the given (size, mtime_ns).
fn unchanged_since(path: &Path, stamp: (u64, i128)) -> bool {
    std::fs::metadata(path).is_ok_and(|m| (m.len(), mtime_ns(&m)) == stamp)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RelinkReport {
    /// `(old sidecar path, new media path)`.
    pub relinked: Vec<(String, String)>,
    /// Orphan sidecars whose hash matches two or more cached items, or that compete with
    /// another orphan for the same single target (all left in place).
    pub ambiguous: Vec<String>,
    /// Orphan sidecars with no usable match: malformed, no hash, no cached item, or the
    /// matched item's file is gone (all left in place).
    pub unmatched: Vec<String>,
    /// Orphan sidecars with exactly one match that cannot be used: the target already has a
    /// sidecar on disk, the target file changed since it was hashed, or the move failed with an
    /// I/O error (left in place).
    pub conflicts: Vec<String>,
}

/// Moves each orphaned sidecar (its media file is gone) next to the one cached item with
/// the same content hash and scope, provided that item has no sidecar on disk and its file
/// still matches the stored hash's size/mtime. Orphans are grouped first: several orphans
/// claiming one target are all `ambiguous` and nothing is moved. A move is journaled and
/// the item's cached sidecar snapshot refreshed.
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
        let Some(media) = media_path_for_sidecar(entry.path()) else {
            continue;
        };
        match media.symlink_metadata() {
            Err(e) if e.kind() == ErrorKind::NotFound => orphans.push(rel),
            Err(e) => tracing::warn!(sidecar = %rel, error = %e, "cannot stat media; skipping"),
            Ok(_) => {}
        }
    }

    let mut report = RelinkReport::default();
    if orphans.is_empty() {
        return Ok(report);
    }

    // (hash, scope) -> items whose stored hash is still current for their record.
    let mut index: HashMap<(String, String), Vec<(String, StoredHash)>> = HashMap::new();
    for path in cache.all_paths()? {
        let Some(rec) = cache.get_by_path(&path)? else {
            continue;
        };
        if let Some(h) = rec.hash
            && h.size == rec.size
            && h.mtime_ns == rec.mtime_ns
        {
            index
                .entry((h.hash.clone(), h.scope.clone()))
                .or_default()
                .push((path, h));
        }
    }

    // Phase 1: classify without moving anything.
    let mut single: Vec<(String, String, StoredHash)> = Vec::new(); // (orphan, target, hash)
    for old in orphans {
        let sc = match Sidecar::read(&root.join(&old)) {
            Ok(Some(sc)) => sc,
            Ok(None) => {
                tracing::debug!(sidecar = %old, "orphan sidecar vanished");
                continue;
            }
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
        match index
            .get(&(hash.to_string(), scope.to_string()))
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            [] => report.unmatched.push(old),
            [(target, h)] => single.push((old, target.clone(), h.clone())),
            _ => report.ambiguous.push(old),
        }
    }
    let mut claims: HashMap<&str, usize> = HashMap::new();
    for (_, target, _) in &single {
        *claims.entry(target.as_str()).or_default() += 1;
    }
    let contested: Vec<bool> = single
        .iter()
        .map(|(_, t, _)| claims[t.as_str()] > 1)
        .collect();
    let mut uncontested = Vec::new();
    for ((old, target, h), c) in single.into_iter().zip(contested) {
        if c {
            report.ambiguous.push(old);
        } else {
            uncontested.push((old, target, h));
        }
    }

    // Phase 2: move.
    let journal = Journal::new(library);
    for (old, target, stored) in uncontested {
        let target_abs = root.join(&target);
        match std::fs::metadata(&target_abs) {
            Ok(m) if (m.len(), mtime_ns(&m)) == (stored.size, stored.mtime_ns) => {}
            Ok(_) => {
                tracing::warn!(sidecar = %old, %target, "target changed since it was hashed; not relinking");
                report.conflicts.push(old);
                continue;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                report.unmatched.push(old);
                continue;
            }
            Err(e) => {
                tracing::warn!(sidecar = %old, %target, error = %e, "cannot stat target; not relinking");
                report.conflicts.push(old);
                continue;
            }
        }
        let new_sidecar_abs = sidecar_path_for(&target_abs);
        let new_sidecar = rel_lossy(root, &new_sidecar_abs);
        let old_abs = root.join(&old);
        // Check the filesystem, not the cached snapshot: the sidecar may be newer than the scan.
        if new_sidecar_abs.symlink_metadata().is_ok() {
            if same_file(&old_abs, &new_sidecar_abs) {
                // A crash between link and remove of an earlier relink: finish the move.
                if let Err(e) = std::fs::remove_file(&old_abs) {
                    tracing::warn!(sidecar = %old, error = %e, "could not finish interrupted relink");
                    report.conflicts.push(old);
                    continue;
                }
            } else {
                tracing::warn!(sidecar = %old, %target, "target already has a sidecar; not relinking");
                report.conflicts.push(old);
                continue;
            }
        } else if let Err(e) = move_no_clobber(&old_abs, &new_sidecar_abs) {
            tracing::warn!(sidecar = %old, %target, error = %e, "could not move sidecar");
            report.conflicts.push(old);
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
        report.relinked.push((old, target));
    }
    cache.flush()?;
    Ok(report)
}

fn refresh_snapshot(library: &Library, cache: &Cache, media: &str) -> Result<(), ServerError> {
    let (snap, err) = match library.read_sidecar(Path::new(media)) {
        Ok(Some(sc)) => (SidecarSnapshot::from(&sc), None),
        Ok(None) => (SidecarSnapshot::default(), None),
        // Only the reason: the Display form carries the absolute sidecar path.
        Err(FwError::MalformedSidecar { reason, .. }) => (SidecarSnapshot::default(), Some(reason)),
        Err(e) => {
            // Leave the record alone; the next scan looks at the sidecar again.
            tracing::warn!(%media, error = %e, "cannot read the relinked sidecar");
            return Ok(());
        }
    };
    cache.set_sidecar(media, snap, err)?;
    Ok(())
}

/// Whether both paths are hard links to the same file (unix; `false` elsewhere).
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

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
fn move_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{MediaFilter, scan};
    use asteria_framework::MediaKind;
    use std::time::{Duration, SystemTime};

    struct All;
    impl MediaFilter for All {
        fn accepts(&self, _: MediaKind) -> bool {
            true
        }
    }

    fn setup(names: &[&str]) -> (tempfile::TempDir, Library, Cache) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        std::fs::create_dir(&root).unwrap();
        let lib = Library::init(&root, "o").unwrap();
        let cache = Cache::open(&dir.path().join("c"), &lib.config().id).unwrap();
        for (i, n) in names.iter().enumerate() {
            std::fs::write(root.join(n), format!("content {i}")).unwrap();
        }
        scan(&lib, &cache, &All).unwrap();
        (dir, lib, cache)
    }

    fn touch(p: &Path) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(500))
            .unwrap();
    }

    #[test]
    fn cancel_midway_stops_further_hashing() {
        let (_d, lib, cache) = setup(&["a.bin", "b.bin", "c.bin"]);
        let cancel = AtomicBool::new(false);
        let n = hash_pending_with(&lib, &cache, &cancel, &mut |_| {
            cancel.store(true, Ordering::Relaxed)
        })
        .unwrap();
        assert_eq!(n, 1);
        let hashed = ["a.bin", "b.bin", "c.bin"]
            .iter()
            .filter(|p| cache.get_by_path(p).unwrap().unwrap().hash.is_some())
            .count();
        assert_eq!(hashed, 1);
    }

    #[test]
    fn file_changed_during_hashing_is_not_stored() {
        let (_d, lib, cache) = setup(&["a.bin", "b.bin"]);
        let root = lib.root().to_path_buf();
        let n = hash_pending_with(&lib, &cache, &AtomicBool::new(false), &mut |p| {
            if p == "a.bin" {
                touch(&root.join(p));
            }
        })
        .unwrap();
        assert_eq!(n, 1);
        assert!(cache.get_by_path("a.bin").unwrap().unwrap().hash.is_none());
        assert!(cache.get_by_path("b.bin").unwrap().unwrap().hash.is_some());
    }

    #[test]
    fn write_does_not_clobber_concurrent_snapshot_change() {
        let (_d, lib, cache) = setup(&["a.bin"]);
        let snap = SidecarSnapshot {
            owner_rating: Some(3),
            has_sidecar: true,
            ..Default::default()
        };
        let n = hash_pending_with(&lib, &cache, &AtomicBool::new(false), &mut |p| {
            cache.set_sidecar(p, snap.clone(), None).unwrap();
        })
        .unwrap();
        assert_eq!(n, 1);
        let rec = cache.get_by_path("a.bin").unwrap().unwrap();
        assert!(rec.hash.is_some());
        assert_eq!(rec.sidecar, snap);
    }

    #[test]
    fn refreshed_malformed_sidecar_keeps_only_the_reason() {
        let (_d, lib, cache) = setup(&["a.bin"]);
        let sidecar = sidecar_path_for(&lib.root().join("a.bin"));
        std::fs::write(&sidecar, "<not xmp").unwrap();
        refresh_snapshot(&lib, &cache, "a.bin").unwrap();
        let err = cache
            .get_by_path("a.bin")
            .unwrap()
            .unwrap()
            .sidecar_error
            .expect("the sidecar is malformed");
        let root = lib.root().to_string_lossy().into_owned();
        assert!(!err.contains(&root), "{err}");
        assert!(!err.contains("a.bin"), "{err}");
    }

    #[test]
    fn fallback_decision() {
        use std::io::{Error, ErrorKind::*};
        assert!(!should_fallback(&Error::from(AlreadyExists)));
        assert!(should_fallback(&Error::from(PermissionDenied)));
        assert!(should_fallback(&Error::from(Unsupported)));
    }

    #[test]
    fn unchanged_since_compares_size_and_mtime() {
        let (_d, lib, _c) = setup(&["a.bin"]);
        let p = lib.root().join("a.bin");
        let m = std::fs::metadata(&p).unwrap();
        let stamp = (m.len(), mtime_ns(&m));
        assert!(unchanged_since(&p, stamp));
        assert!(!unchanged_since(&p, (stamp.0 + 1, stamp.1)));
        touch(&p);
        assert!(!unchanged_since(&p, stamp));
        assert!(!unchanged_since(&lib.root().join("missing"), stamp));
    }
}
