//! Moving, renaming and trashing one media file together with its sidecar: the per-operation
//! core of the organize batches (run as Journal operations, see [`crate::journal::files`]),
//! request validation, dry runs, and the batches' results, status and history as the
//! Journal records them.
//!
//! Writes are ordered original, cache, journal. A move never replaces an existing file, and
//! a stop at any point never loses an original: after the media moved but before the sidecar
//! did, `relink_orphans` finds the sidecar again; a stop before the Journal records the move
//! only loses the undo of that one move; a stale cache is fixed by the next scan.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::Path;

use asteria_common_api::{
    BatchSummary, ItemId, MAX_LABEL_CHARS, MAX_OPS, OpResult, OpStatus, OrganizeAction,
    OrganizeHistory, OrganizeOp, OrganizeStatus, RpcError, SkipReason, codes,
};
use asteria_common_core::Actor;
use asteria_common_core::hash::file_hash;
use asteria_common_core::paths::{META_DIR, TRASH_DIR, sidecar_path_for, trash_rel};
use asteria_common_core::upload_path::validate_inbox_path;
use asteria_common_core::{Error as FwError, Library, classify};
use chrono::{DateTime, SecondsFormat};
use corundum_framework::journal::{
    BatchId, BatchRecords, BatchSummary as JournalSummary, JournalError, OpKind as _, OpRecord,
    Outcome,
};
use grain_id::GrainId;

use crate::cache::{Cache, ItemRecord, SidecarSnapshot, StoredHash, item_id_for};
use crate::ext::{AppExtension, ExtContext, Rewrite};
use crate::fsutil::{DirError, check_dirs_inside, create_dirs_inside, move_no_clobber};
use crate::journal::files::{Expect, MoveDone, planned_op};
use crate::journal::map::{reason_of, rpc_error as journal_rpc_error};
use crate::journal::{ServerJournal, files};
use crate::rpc::rpc_error;
use crate::scan::{extract_guarded, mtime_ns};

/// One planned operation: move `from` to `to`, provided the file is still what was planned.
#[derive(Debug, Clone)]
pub(crate) struct PlannedOp {
    pub from: String,
    pub to: Target,
    pub expect: Expect,
}

#[derive(Debug, Clone)]
pub(crate) enum Target {
    Path(String),
    /// Into the trash, under this id (`trash_rel`).
    Trash(GrainId),
}

#[derive(Debug, Clone)]
pub(crate) struct Done {
    /// Where the file went; the trash path for a trashed file.
    pub to: String,
    /// The new record, when the file is now somewhere the library shows.
    pub record: Option<ItemRecord>,
    /// The file's hash for its stamp, when known: kept for a restore from the trash.
    pub hash: Option<StoredHash>,
}

pub(crate) struct OpCtx<'a> {
    pub library: &'a Library,
    pub cache: &'a Cache,
    pub ext: &'a dyn AppExtension,
}

/// The effect of the earlier operations of a batch, for checks that do not touch the disk.
#[derive(Debug, Default)]
pub(crate) struct Sim {
    taken: HashSet<String>,
    freed: HashSet<String>,
}

impl Sim {
    /// Notes that `from` was moved to `to` (media paths; the sidecar follows its media).
    /// Notes that `from` was moved away to somewhere that cannot clash (the trash).
    pub(crate) fn free(&mut self, from: &str) {
        self.taken.remove(from);
        self.freed.insert(from.to_string());
    }

    pub(crate) fn record(&mut self, from: &str, to: &str) {
        self.taken.remove(from);
        self.freed.insert(from.to_string());
        self.freed.remove(to);
        self.taken.insert(to.to_string());
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum FailAt {
    AfterMedia,
    AfterSidecar,
    /// Not a stop: a foreign sidecar appears at the target after the media moved.
    PlantTargetSidecar,
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_AT: std::cell::Cell<Option<FailAt>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn stop_at(at: FailAt) -> Result<(), SkipReason> {
    if FAIL_AT.with(|f| f.get()) == Some(at) {
        return Err(SkipReason::Io);
    }
    Ok(())
}

/// A move that waits, on whatever thread runs it (the Journal's worker), before it touches
/// anything: a test stops a batch midway with it, as a crash would.
#[cfg(test)]
pub(crate) mod hold {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender, channel};

    struct Held {
        source: PathBuf,
        entered: Sender<()>,
        release: Receiver<()>,
    }

    static HELD: Mutex<Option<Held>> = Mutex::new(None);

    /// The move of the absolute `source` says so on `entered`, waits for `release`, then
    /// gives up.
    pub(crate) struct Hold {
        pub entered: Receiver<()>,
        pub release: Sender<()>,
    }

    pub(crate) fn at(source: PathBuf) -> Hold {
        let (entered, entered_rx) = channel();
        let (release_tx, release) = channel();
        *HELD.lock().unwrap() = Some(Held {
            source,
            entered,
            release,
        });
        Hold {
            entered: entered_rx,
            release: release_tx,
        }
    }

    /// `true` when the move of `source` was held and must give up.
    pub(crate) fn wait(source: &Path) -> bool {
        let held = {
            let mut slot = HELD.lock().unwrap_or_else(|p| p.into_inner());
            match &*slot {
                Some(h) if h.source == source => slot.take(),
                _ => None,
            }
        };
        let Some(h) = held else {
            return false;
        };
        let _ = h.entered.send(());
        let _ = h.release.recv();
        true
    }
}

fn in_meta_dir(rel: &str) -> bool {
    rel == META_DIR
        || rel
            .strip_prefix(META_DIR)
            .is_some_and(|r| r.starts_with('/'))
}

fn io_skip(what: &str, rel: &str, e: &std::io::Error) -> SkipReason {
    tracing::warn!(path = %rel, error = %e, "{what}");
    SkipReason::Io
}

/// Checks `op` without changing anything: `Ok` when `apply` would go ahead (barring a race).
/// `sim` stands for the earlier operations of the same batch.
pub(crate) fn check(ctx: &OpCtx, op: &PlannedOp, sim: &Sim) -> Result<(), SkipReason> {
    check_source(ctx, op)?;
    // A trash destination is new for every operation, so it is always free.
    if let Target::Path(to) = &op.to {
        let root = ctx.library.root();
        check_dirs_inside(root, folder_of(to)).map_err(|e| dir_skip(to, e))?;
        let (from_abs, to_abs) = (root.join(&op.from), root.join(to));
        let taken = |from: &Path, to: &Path| -> std::io::Result<bool> {
            Ok(exists(to)? && !is_case_variant(from, to))
        };
        let blocked = sim.taken.contains(to)
            || (!sim.freed.contains(to)
                && (taken(&from_abs, &to_abs)
                    .map_err(|e| io_skip("cannot stat target", to, &e))?
                    || taken(&sidecar_path_for(&from_abs), &sidecar_path_for(&to_abs))
                        .map_err(|e| io_skip("cannot stat target sidecar", to, &e))?));
        if blocked {
            return Err(SkipReason::TargetExists);
        }
    }
    Ok(())
}

/// The folder part of the library-relative `rel` (`""` at the top).
fn folder_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// A destination folder that is a link is [`SkipReason::Symlink`]: going through it could
/// put the file outside the library (#32).
fn dir_skip(to: &str, e: DirError) -> SkipReason {
    match e {
        DirError::Linked(dir) => {
            tracing::warn!(path = %to, folder = %dir, "the target folder is a link");
            SkipReason::Symlink
        }
        DirError::Io(e) => io_skip("cannot check target folder", to, &e),
    }
}

/// Whether `to` is `from` itself under a name that differs only in case: one file on a
/// case-insensitive filesystem (#33). Such a rename goes through a temporary name.
fn is_case_variant(from: &Path, to: &Path) -> bool {
    let (Some(f), Some(t)) = (from.file_name(), to.file_name()) else {
        return false;
    };
    f != t
        && from.parent() == to.parent()
        && f.to_string_lossy().to_lowercase() == t.to_string_lossy().to_lowercase()
        && same_file::is_same_file(from, to).unwrap_or(false)
}

/// Moves `from` to `to` without replacing anything; through a temporary name beside `to`
/// when `to` is `from` itself in another case, which a move straight there would find taken.
/// The temporary name is visible, and keeps the extension, so that a file left there by a
/// crash is found by the next scan.
fn move_media(from: &Path, to: &Path, tag: &GrainId) -> std::io::Result<()> {
    if !is_case_variant(from, to) {
        return move_no_clobber(from, to);
    }
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned());
    let tmp = to.with_file_name(format!("asteria-rename-{tag}-{}", name.unwrap_or_default()));
    move_no_clobber(from, &tmp)?;
    move_no_clobber(&tmp, to).inspect_err(|_| {
        if let Err(e) = move_no_clobber(&tmp, from) {
            tracing::error!(from = %from.display(), tmp = %tmp.display(), error = %e,
                "could not put a file back after a case-only rename failed");
        }
    })
}

pub(crate) fn exists(p: &Path) -> std::io::Result<bool> {
    match p.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Steps 1 to 3: the source is a regular file that still has the planned size, time and hash.
fn check_source(ctx: &OpCtx, op: &PlannedOp) -> Result<(), SkipReason> {
    let abs = ctx.library.root().join(&op.from);
    let meta = match abs.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err(SkipReason::Missing),
        Err(e) => return Err(io_skip("cannot stat source", &op.from, &e)),
    };
    if meta.file_type().is_symlink() {
        return Err(SkipReason::Symlink);
    }
    if !meta.is_file() {
        return Err(SkipReason::Missing);
    }
    let stamp = (meta.len(), mtime_ns(&meta));
    if stamp != (op.expect.size, op.expect.mtime_ns) {
        return Err(SkipReason::Changed);
    }
    if let Some(want) = &op.expect.file_hash {
        let cached = ctx
            .cache
            .get_by_path(&op.from)
            .ok()
            .flatten()
            .and_then(|r| r.hash)
            .filter(|h| (h.size, h.mtime_ns) == stamp)
            .map(|h| h.file_hash);
        let have = match cached {
            Some(h) => h,
            None => file_hash(&abs)
                .map_err(|e| {
                    tracing::warn!(path = %op.from, error = %e, "cannot hash source");
                    SkipReason::Io
                })?
                .to_string(),
        };
        if &have != want {
            return Err(SkipReason::HashMismatch);
        }
    }
    Ok(())
}

/// Carries `op` out: the media file and its sidecar move, and the cache follows. Checks
/// again against the disk first; never replaces an existing file.
pub(crate) fn apply(ctx: &OpCtx, op: &PlannedOp) -> Result<Done, SkipReason> {
    let root = ctx.library.root();
    #[cfg(test)]
    if hold::wait(&root.join(&op.from)) {
        return Err(SkipReason::Io);
    }
    check(ctx, op, &Sim::default())?;
    // Names the temporary file of a case-only rename.
    let tag = GrainId::random();
    let to = match &op.to {
        Target::Path(p) => p.clone(),
        Target::Trash(id) => trash_rel(&id.to_string(), &op.from),
    };
    let from_abs = root.join(&op.from);
    let to_abs = root.join(&to);
    let from_sidecar = sidecar_path_for(&from_abs);
    let to_sidecar = sidecar_path_for(&to_abs);
    let has_sidecar =
        exists(&from_sidecar).map_err(|e| io_skip("cannot stat sidecar", &op.from, &e))?;

    create_dirs_inside(root, folder_of(&to)).map_err(|e| match e {
        DirError::Io(e) => io_skip("cannot create folder", &to, &e),
        linked => dir_skip(&to, linked),
    })?;
    move_media(&from_abs, &to_abs, &tag).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => SkipReason::TargetExists,
        ErrorKind::NotFound => SkipReason::Missing,
        _ => io_skip("cannot move media", &op.from, &e),
    })?;
    #[cfg(test)]
    stop_at(FailAt::AfterMedia)?;
    #[cfg(test)]
    if FAIL_AT.with(|f| f.get()) == Some(FailAt::PlantTargetSidecar) {
        std::fs::write(&to_sidecar, "foreign sidecar").unwrap();
    }
    if has_sidecar && let Err(e) = move_media(&from_sidecar, &to_sidecar, &tag) {
        // Never leave the media moved with its sidecar behind.
        if let Err(back) = move_media(&to_abs, &from_abs, &tag) {
            tracing::error!(from = %op.from, %to, error = %back,
                "could not put the media back after its sidecar failed to move");
        }
        return Err(match e.kind() {
            ErrorKind::AlreadyExists => SkipReason::TargetExists,
            _ => io_skip("cannot move sidecar", &op.from, &e),
        });
    }
    #[cfg(test)]
    stop_at(FailAt::AfterSidecar)?;

    let stamp = (op.expect.size, op.expect.mtime_ns);
    let hash = ctx
        .cache
        .get_by_path(&op.from)
        .ok()
        .flatten()
        .and_then(|r| r.hash)
        .filter(|h| (h.size, h.mtime_ns) == stamp)
        .or_else(|| {
            op.expect
                .hash
                .clone()
                .filter(|h| (h.size, h.mtime_ns) == stamp)
        });
    let record = update_cache(ctx, op, &to);
    Ok(Done { to, record, hash })
}

/// Brings the cache in line with the move. A failure is only logged: the next scan fixes it.
fn update_cache(ctx: &OpCtx, op: &PlannedOp, to: &str) -> Option<ItemRecord> {
    let from_visible = !in_meta_dir(&op.from);
    let to_visible = !in_meta_dir(to);
    let result = match (from_visible, to_visible) {
        (true, true) => ctx.cache.move_item(&op.from, to).map_err(|e| e.to_string()),
        (true, false) => ctx
            .cache
            .remove(&op.from)
            .map(|()| None)
            .map_err(|e| e.to_string()),
        (false, true) => restore_record(ctx, op, to),
        (false, false) => Ok(None),
    };
    result.unwrap_or_else(|e| {
        tracing::warn!(from = %op.from, %to, error = %e, "cache not updated; the next scan will fix it");
        None
    })
}

/// A new record for a file taken out of the trash, built the way a scan builds one.
fn restore_record(ctx: &OpCtx, op: &PlannedOp, to: &str) -> Result<Option<ItemRecord>, String> {
    let abs = ctx.library.root().join(to);
    let kind = classify(&abs).map_err(|e| e.to_string())?;
    if !ctx.ext.accepts(kind) {
        return Ok(None);
    }
    let meta = std::fs::metadata(&abs).map_err(|e| e.to_string())?;
    let (size, mtime) = (meta.len(), mtime_ns(&meta));
    let (sidecar, sidecar_error) = match ctx.library.read_sidecar(Path::new(to)) {
        Ok(Some(sc)) => (SidecarSnapshot::from(&sc), None),
        Ok(None) => (SidecarSnapshot::default(), None),
        Err(FwError::MalformedSidecar { reason, .. }) => (SidecarSnapshot::default(), Some(reason)),
        Err(e) => return Err(e.to_string()),
    };
    let rec = ItemRecord {
        id: item_id_for(&ctx.library.config().id, to),
        path: to.to_string(),
        kind,
        size,
        mtime_ns: mtime,
        hash: op
            .expect
            .hash
            .clone()
            .filter(|h| (h.size, h.mtime_ns) == (size, mtime)),
        sidecar,
        sidecar_error,
    };
    let data = extract_guarded(ctx.ext, to, &abs);
    ctx.cache
        .put_many_with_app_data(std::slice::from_ref(&rec), &[(to.to_string(), data)])
        .map_err(|e| e.to_string())?;
    Ok(Some(rec))
}

/// An operation of a request after the acceptance checks: ready to run, or already skipped
/// (the path is what `OpResult::path` reports; empty when the item is unknown).
pub(crate) type Planned = Result<PlannedOp, (String, SkipReason)>;

/// What a batch reports.
pub struct Batch {
    /// `None` for a dry run.
    pub id: Option<BatchId>,
    pub results: Vec<OpResult>,
    /// Not over yet; `results` is empty.
    pub running: bool,
}

fn invalid(msg: impl Into<String>) -> RpcError {
    rpc_error(codes::INVALID_PARAMS, msg)
}

/// A label is at most `MAX_LABEL_CHARS` characters without control characters.
pub(crate) fn validate_label(label: Option<&str>) -> Result<(), RpcError> {
    if let Some(l) = label
        && (l.chars().count() > MAX_LABEL_CHARS || l.chars().any(char::is_control))
    {
        return Err(invalid(format!(
            "label is at most {MAX_LABEL_CHARS} characters without control characters"
        )));
    }
    Ok(())
}

fn extension_of(path: &str) -> Option<&str> {
    Path::new(path).extension().and_then(|e| e.to_str())
}

/// Checks the whole request and turns it into planned operations. Any problem with the
/// request itself is an error and nothing runs; an unknown item only skips its operation.
pub(crate) fn plan_apply(cache: &Cache, ops: &[OrganizeOp]) -> Result<Vec<Planned>, RpcError> {
    if ops.len() > MAX_OPS {
        return Err(invalid(format!("at most {MAX_OPS} operations per batch")));
    }
    let mut seen_items = HashSet::new();
    let mut seen_targets: HashMap<String, usize> = HashMap::new();
    let mut planned = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        if !seen_items.insert(op.item_id.0.as_str()) {
            return Err(invalid(format!("item {i} appears twice")));
        }
        let rec = cache.get_by_id(&op.item_id).map_err(|e| {
            tracing::error!(error = %e, "organize: cache read failed");
            rpc_error(codes::INTERNAL_ERROR, "internal error")
        })?;
        let target = match &op.action {
            OrganizeAction::Move { to } => {
                validate_inbox_path(to).map_err(|m| rpc_error(codes::INVALID_PATH, m))?;
                // Without regard to case: on a case-insensitive filesystem (macOS's and
                // Windows's by default) such targets are one file (#33).
                if let Some(j) = seen_targets.insert(to.to_lowercase(), i) {
                    return Err(invalid(format!(
                        "operations {j} and {i} have the same target"
                    )));
                }
                if let Some(rec) = &rec {
                    let (a, b) = (extension_of(&rec.path), extension_of(to));
                    let same = match (a, b) {
                        (None, None) => true,
                        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                        _ => false,
                    };
                    if !same {
                        return Err(invalid(format!("operation {i} changes the extension")));
                    }
                }
                Target::Path(to.clone())
            }
            OrganizeAction::Trash => Target::Trash(GrainId::random()),
        };
        planned.push(match rec {
            None => Err((String::new(), SkipReason::Missing)),
            Some(rec) => Ok(PlannedOp {
                from: rec.path,
                to: target,
                expect: Expect {
                    size: rec.size,
                    mtime_ns: rec.mtime_ns,
                    file_hash: op.file_hash.clone(),
                    hash: rec.hash,
                },
            }),
        });
    }
    Ok(planned)
}

pub(crate) fn skipped(index: usize, path: &str, reason: SkipReason) -> OpResult {
    OpResult {
        index: index as u32,
        status: OpStatus::Skipped,
        path: path.to_string(),
        new_path: None,
        new_item_id: None,
        reason: Some(reason),
    }
}

pub(crate) fn done(
    index: usize,
    path: &str,
    new_path: Option<String>,
    new_item_id: Option<ItemId>,
) -> OpResult {
    OpResult {
        index: index as u32,
        status: OpStatus::Done,
        path: path.to_string(),
        new_path,
        new_item_id,
        reason: None,
    }
}

/// The path an operation reports, never one inside `.asteria`: a restore from the trash
/// reports the place the item goes back to.
pub(crate) fn reported_path(op: &PlannedOp) -> &str {
    match &op.to {
        Target::Path(to) if in_meta_dir(&op.from) => to,
        _ => &op.from,
    }
}

/// The report of an operation that was carried out, to `to`. A move into the trash (the
/// redo of a trash) reports like a trash: no new path or id.
fn applied(index: usize, op: &PlannedOp, to: String, item_id: Option<ItemId>) -> OpResult {
    let path = reported_path(op);
    match &op.to {
        Target::Path(p) if !in_meta_dir(p) => done(index, path, Some(to), item_id),
        _ => done(index, path, None, None),
    }
}

/// Checks every operation in order without touching anything; each one sees the effect the
/// earlier ones would have. A hash mismatch reports as `changed`, as a real apply's does
/// (API 2.5 no longer sends `hash_mismatch`).
pub(crate) fn dry_run(ctx: &OpCtx, ops: &[Planned]) -> Vec<OpResult> {
    let mut sim = Sim::default();
    let lib_id = &ctx.library.config().id;
    ops.iter()
        .enumerate()
        .map(|(i, planned)| match planned {
            Err((path, why)) => skipped(i, path, *why),
            Ok(op) => match check(ctx, op, &sim) {
                Err(SkipReason::HashMismatch) => skipped(i, reported_path(op), SkipReason::Changed),
                Err(why) => skipped(i, reported_path(op), why),
                Ok(()) => match &op.to {
                    Target::Path(to) => {
                        sim.record(&op.from, to);
                        // A move into the trash (the redo of a trash) reports like a trash.
                        let (new_path, id) = if in_meta_dir(to) {
                            (None, None)
                        } else {
                            (Some(to.clone()), Some(item_id_for(lib_id, to)))
                        };
                        done(i, reported_path(op), new_path, id)
                    }
                    Target::Trash(_) => {
                        sim.free(&op.from);
                        done(i, reported_path(op), None, None)
                    }
                },
            },
        })
        .collect()
}

/// The user an actor acts for.
pub(crate) fn actor_user(actor: Actor) -> asteria_common_core::UserId {
    match actor {
        Actor::User(u) => u,
        Actor::Agent { on_behalf_of } => on_behalf_of,
    }
}

/// Asks the app what else the moves change. A panicking extension gives nothing.
pub(crate) fn rewrites_after(
    ext: &dyn AppExtension,
    ctx: &ExtContext,
    moves: &[(String, String)],
) -> Vec<Rewrite> {
    if moves.is_empty() {
        return Vec::new();
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ext.after_moves(ctx, moves)))
        .unwrap_or_else(|_| {
            tracing::error!("after_moves panicked; no playlists were rewritten");
            Vec::new()
        })
}

/// Removes the folders that `sources` left empty, up towards the root. The library root,
/// `.asteria` and the trash folder stay.
pub(crate) fn remove_emptied_folders(root: &Path, sources: &[String]) {
    let keep = format!("{META_DIR}/{TRASH_DIR}");
    for src in sources {
        let mut dir = src.rsplit_once('/').map(|p| p.0);
        while let Some(rel) = dir {
            if rel == META_DIR || rel == keep {
                break;
            }
            // Fails while the folder still holds something (or on any other error): stop.
            if std::fs::remove_dir(root.join(rel)).is_err() {
                break;
            }
            dir = rel.rsplit_once('/').map(|p| p.0);
        }
    }
}

/// A library path the app may rewrite: relative, plain, and not inside `.asteria`.
pub(crate) fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !in_meta_dir(rel)
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The operations of a batch that answer to the request's: its own `move` and `trash`, in
/// order (an undo's included), not their children nor the closing tidy.
fn file_ops(b: &BatchRecords) -> impl DoubleEndedIterator<Item = &OpRecord> {
    b.ops.iter().filter(|o| {
        o.parent.is_none() && (o.kind == files::Move::KIND || o.kind == files::Trash::KIND)
    })
}

fn done_of(rec: &OpRecord) -> Option<MoveDone> {
    match &rec.outcome {
        Outcome::Done(v) => serde_json::from_value(v.clone()).ok(),
        Outcome::Skipped(_) => None,
    }
}

/// The report of a recorded `move` or `trash`. An operation whose plan cannot be read (an
/// undo the Journal could not plan) reports no path.
fn result_of(index: usize, rec: &OpRecord) -> OpResult {
    let op = planned_op(&rec.kind, &rec.plan);
    let path = op.as_ref().map_or("", reported_path);
    match (&rec.outcome, &op, done_of(rec)) {
        (Outcome::Skipped(s), _, _) => skipped(index, path, reason_of(s)),
        (Outcome::Done(_), Some(op), Some(d)) => applied(index, op, d.to, d.item_id.map(ItemId)),
        (Outcome::Done(_), _, _) => done(index, path, None, None),
    }
}

/// The reports of the batch's operations, numbered in order.
pub(crate) fn results(b: &BatchRecords) -> Vec<OpResult> {
    file_ops(b)
        .enumerate()
        .map(|(i, rec)| result_of(i, rec))
        .collect()
}

/// The reports of an `organize.apply` batch: the operations it ran under their request
/// index (`indices`, in order), with the ones skipped before it ran (`early`).
pub(crate) fn merge_results(
    b: &BatchRecords,
    indices: &[usize],
    early: Vec<OpResult>,
) -> Vec<OpResult> {
    let mut ran = file_ops(b);
    let mut out: Vec<OpResult> = indices
        .iter()
        .map(|&i| match ran.next() {
            Some(rec) => result_of(i, rec),
            // The batch ended without it (its record could not be written).
            None => skipped(i, "", SkipReason::Io),
        })
        .collect();
    out.extend(early);
    out.sort_by_key(|r| r.index);
    out
}

/// The moves that would undo the batch, newest first, for a dry run.
pub(crate) fn undo_plans(b: &BatchRecords) -> Vec<Planned> {
    file_ops(b)
        .rev()
        .filter(|rec| matches!(rec.outcome, Outcome::Done(_)))
        .map(|rec| {
            let back = done_of(rec).map(|d| d.back().op());
            back.ok_or_else(|| (String::new(), SkipReason::Io))
        })
        .collect()
}

/// The places the batch's operations moved files from, for the folders they may have left
/// empty.
pub(crate) fn sources(b: &BatchRecords) -> Vec<String> {
    file_ops(b).filter_map(done_of).map(|d| d.from).collect()
}

/// Whether the batch ran to its end; one ended when the store was opened again did not.
fn ran_to_end(s: &JournalSummary) -> bool {
    s.complete && !s.interrupted
}

fn summary_of(s: &JournalSummary) -> BatchSummary {
    let at = DateTime::from_timestamp_millis(s.begin.at)
        .unwrap_or(DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::AutoSi, true);
    BatchSummary {
        batch_id: s.begin.batch_id.to_string(),
        user: s.begin.actor.principal.clone(),
        agent: s.begin.actor.agent,
        at,
        label: s.begin.label.clone(),
        op_count: s.done + s.skipped,
        done: s.done,
        skipped: s.skipped,
        complete: ran_to_end(s),
        // A redo undoes the undo it names.
        undo_of: s.begin.undo_of.or(s.begin.redo_of).map(|b| b.to_string()),
        undone_by: s.undone_by.map(|b| b.to_string()),
    }
}

/// Whether the batch has one of its own operations of these kinds.
fn has_kind(
    journal: &ServerJournal,
    s: &JournalSummary,
    kinds: &[String],
) -> Result<bool, RpcError> {
    match journal.status(s.begin.batch_id) {
        Ok(b) => Ok(b
            .ops
            .iter()
            .any(|o| o.parent.is_none() && kinds.contains(&o.kind))),
        // Dropped from the store meanwhile.
        Err(JournalError::NotFound(_)) => Ok(false),
        Err(e) => Err(journal_rpc_error(e)),
    }
}

/// One page of the history, newest first: at most `limit` batches older than `cursor`,
/// only those with an operation of `kinds` when given. Blocks on the Journal.
pub(crate) fn history(
    journal: &ServerJournal,
    limit: u32,
    cursor: Option<&str>,
    kinds: Option<&[String]>,
) -> Result<OrganizeHistory, RpcError> {
    let unknown = || invalid("unknown cursor");
    let mut cursor = match cursor {
        None => None,
        Some(c) => Some(BatchId(c.parse::<GrainId>().map_err(|_| unknown())?)),
    };
    // One more than asked, to tell whether there is a next page.
    let want = limit as usize + 1;
    let mut found: Vec<BatchSummary> = Vec::new();
    while found.len() < want {
        let page = journal.history(cursor, want).map_err(|e| match e {
            JournalError::NotFound(_) => unknown(),
            e => journal_rpc_error(e),
        })?;
        let Some(last) = page.last() else { break };
        cursor = Some(last.begin.batch_id);
        let end = page.len() < want;
        for s in &page {
            if found.len() == want {
                break;
            }
            let keep = match kinds {
                None => true,
                Some(kinds) => has_kind(journal, s, kinds)?,
            };
            if keep {
                found.push(summary_of(s));
            }
        }
        if end {
            break;
        }
    }
    let more = found.len() > limit as usize;
    found.truncate(limit as usize);
    let next_cursor = if more {
        found.last().map(|b| b.batch_id.clone())
    } else {
        None
    };
    Ok(OrganizeHistory {
        batches: found,
        next_cursor,
    })
}

/// The request indices of an `organize.apply` batch's operations and the operations it
/// skipped while planning, as its closing tidy keeps them. `None` for any other batch, and
/// for one recorded before they were kept.
fn request_order(b: &BatchRecords) -> Option<(Vec<usize>, Vec<OpResult>)> {
    let tidy = b
        .ops
        .iter()
        .find(|o| o.parent.is_none() && o.kind == files::OrganizeTidy::KIND)?;
    let plan: files::TidyPlan = serde_json::from_value(tidy.plan.clone()).ok()?;
    if plan.indices.is_empty() {
        return None;
    }
    let indices = plan.indices.iter().map(|&i| i as usize).collect();
    let early = plan
        .early
        .into_iter()
        .map(|s| skipped(s.index as usize, &s.path, s.reason))
        .collect();
    Some((indices, early))
}

/// Where the batch `id` is, with its results once it has ended. Blocks on the Journal.
pub(crate) fn status(journal: &ServerJournal, id: BatchId) -> Result<OrganizeStatus, JournalError> {
    let b = journal.status(id)?;
    let results = match request_order(&b) {
        Some((indices, early)) => merge_results(&b, &indices, early),
        None => results(&b),
    };
    let count = |status| results.iter().filter(|r| r.status == status).count() as u32;
    Ok(OrganizeStatus {
        batch_id: id.to_string(),
        running: !b.summary.complete,
        complete: ran_to_end(&b.summary),
        done: count(OpStatus::Done),
        skipped: count(OpStatus::Skipped),
        results: b.summary.complete.then_some(results),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::relink_orphans;
    use crate::scan::scan;
    use asteria_common_core::MediaKind;

    struct All;
    impl AppExtension for All {
        fn accepts(&self, _: MediaKind) -> bool {
            true
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        lib: std::sync::Arc<Library>,
        cache: std::sync::Arc<Cache>,
    }

    impl Env {
        fn new() -> Env {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("lib");
            std::fs::create_dir(&root).unwrap();
            let lib = Library::init(&root, "o")
                .unwrap()
                .with_state_dir(dir.path().join("state"));
            let cache = Cache::open(&dir.path().join("c"), &lib.config().id).unwrap();
            std::fs::write(root.join("a.jpg"), "photo a").unwrap();
            std::fs::write(root.join("b.jpg"), "photo b").unwrap();
            lib.update_sidecar(Path::new("a.jpg"), |sc| {
                sc.subjects.insert("x".into());
            })
            .unwrap();
            scan(&lib, &cache, &All).unwrap();
            Env {
                _dir: dir,
                lib: std::sync::Arc::new(lib),
                cache: std::sync::Arc::new(cache),
            }
        }

        fn ctx(&self) -> OpCtx<'_> {
            OpCtx {
                library: &self.lib,
                cache: &self.cache,
                ext: &All,
            }
        }

        fn op(&self, from: &str, to: Target) -> PlannedOp {
            let m = std::fs::metadata(self.lib.root().join(from)).unwrap();
            PlannedOp {
                from: from.into(),
                to,
                expect: Expect {
                    size: m.len(),
                    mtime_ns: mtime_ns(&m),
                    file_hash: None,
                    hash: None,
                },
            }
        }

        fn path(&self, rel: &str) -> std::path::PathBuf {
            self.lib.root().join(rel)
        }
    }

    fn to(p: &str) -> Target {
        Target::Path(p.into())
    }

    #[test]
    fn moves_media_and_sidecar() {
        let e = Env::new();
        let done = apply(&e.ctx(), &e.op("a.jpg", to("sub/c.jpg"))).unwrap();
        assert_eq!(done.to, "sub/c.jpg");
        assert_eq!(done.record.unwrap().path, "sub/c.jpg");
        assert!(!e.path("a.jpg").exists() && !e.path("a.jpg.xmp").exists());
        assert!(e.path("sub/c.jpg").exists() && e.path("sub/c.jpg.xmp").exists());
        assert!(e.cache.get_by_path("sub/c.jpg").unwrap().is_some());
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_none());
    }

    #[test]
    fn a_rename_moves_the_revision() {
        let e = Env::new();
        let before = e.cache.revision();
        apply(&e.ctx(), &e.op("a.jpg", to("c.jpg"))).unwrap();
        assert!(e.cache.revision() > before);
    }

    #[test]
    fn trash_goes_under_the_trash_id_and_leaves_the_cache() {
        let e = Env::new();
        let id = GrainId::random();
        let done = apply(&e.ctx(), &e.op("a.jpg", Target::Trash(id))).unwrap();
        assert_eq!(done.to, format!(".asteria/trash/{id}/a.jpg"));
        assert!(done.record.is_none());
        assert!(e.path(&done.to).exists());
        assert!(e.path(&format!("{}.xmp", done.to)).exists());
        assert!(!e.path("a.jpg").exists());
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_none());
    }

    #[test]
    fn restoring_from_trash_brings_back_the_record_and_hash() {
        let e = Env::new();
        crate::hashing::hash_pending(&e.lib, &e.cache, &Default::default()).unwrap();
        let hashed = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash.unwrap();
        let done = apply(&e.ctx(), &e.op("a.jpg", Target::Trash(GrainId::random()))).unwrap();
        let stored = done.hash.clone().unwrap();
        assert_eq!(stored, hashed);
        let mut back = e.op(&done.to, to("a.jpg"));
        back.expect.hash = Some(stored);
        let restored = apply(&e.ctx(), &back).unwrap();
        let rec = restored.record.unwrap();
        assert_eq!(rec.hash, Some(hashed));
        assert!(rec.sidecar.has_sidecar);
        assert_eq!(e.cache.get_by_path("a.jpg").unwrap().unwrap(), rec);
        assert!(e.path("a.jpg.xmp").exists());
    }

    #[test]
    fn target_or_its_sidecar_taken_is_skipped_untouched() {
        let e = Env::new();
        std::fs::write(e.path("c.jpg"), "other").unwrap();
        let r = apply(&e.ctx(), &e.op("a.jpg", to("c.jpg")));
        assert_eq!(r.unwrap_err(), SkipReason::TargetExists);
        std::fs::remove_file(e.path("c.jpg")).unwrap();
        std::fs::write(e.path("c.jpg.xmp"), "other sidecar").unwrap();
        let r = apply(&e.ctx(), &e.op("a.jpg", to("c.jpg")));
        assert_eq!(r.unwrap_err(), SkipReason::TargetExists);
        assert_eq!(
            std::fs::read(e.path("c.jpg.xmp")).unwrap(),
            b"other sidecar"
        );
        assert_eq!(std::fs::read(e.path("a.jpg")).unwrap(), b"photo a");
        assert!(e.path("a.jpg.xmp").exists() && !e.path("c.jpg").exists());
    }

    /// A rename that changes only the case goes ahead on a case-insensitive filesystem too,
    /// where the target is the source itself (#33): the folder then holds the new names only.
    #[test]
    fn a_case_only_rename_goes_ahead() {
        let e = Env::new();
        let op = e.op("a.jpg", to("A.jpg"));
        assert_eq!(check(&e.ctx(), &op, &Sim::default()), Ok(()));
        let done = apply(&e.ctx(), &op).unwrap();
        assert_eq!(done.to, "A.jpg");
        let mut names: Vec<_> = std::fs::read_dir(e.lib.root())
            .unwrap()
            .map(|d| d.unwrap().file_name().into_string().unwrap())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        assert_eq!(names, ["A.jpg", "A.jpg.xmp", "b.jpg"]);
        assert!(e.cache.get_by_path("A.jpg").unwrap().is_some());
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_none());
    }

    /// A different file whose name differs only in case is still taken (a case-sensitive
    /// filesystem holds both).
    #[test]
    fn a_case_variant_that_is_another_file_is_taken() {
        let e = Env::new();
        let other = e.path("B.jpg");
        if std::fs::write(&other, "photo B").is_err()
            || std::fs::read(e.path("b.jpg")).unwrap() == b"photo B"
        {
            return; // a case-insensitive filesystem: B.jpg is b.jpg
        }
        let r = apply(&e.ctx(), &e.op("b.jpg", to("B.jpg")));
        assert_eq!(r.unwrap_err(), SkipReason::TargetExists);
        assert_eq!(std::fs::read(&other).unwrap(), b"photo B");
        assert_eq!(std::fs::read(e.path("b.jpg")).unwrap(), b"photo b");
    }

    #[test]
    fn changed_missing_and_hash_mismatch() {
        let e = Env::new();
        let stale = e.op("a.jpg", to("c.jpg"));
        let mut wrong = e.op("b.jpg", to("d.jpg"));
        wrong.expect.file_hash = Some("blake3:00".into());
        assert_eq!(
            apply(&e.ctx(), &wrong).unwrap_err(),
            SkipReason::HashMismatch
        );
        let right = file_hash(&e.path("b.jpg")).unwrap().to_string();
        wrong.expect.file_hash = Some(right);
        apply(&e.ctx(), &wrong).unwrap();

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(e.path("a.jpg"))
            .unwrap();
        f.write_all(b"x").unwrap();
        drop(f);
        assert_eq!(apply(&e.ctx(), &stale).unwrap_err(), SkipReason::Changed);
        std::fs::remove_file(e.path("a.jpg")).unwrap();
        assert_eq!(apply(&e.ctx(), &stale).unwrap_err(), SkipReason::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_source_is_skipped() {
        let e = Env::new();
        std::os::unix::fs::symlink("b.jpg", e.path("l.jpg")).unwrap();
        let op = e.op("b.jpg", to("c.jpg"));
        let link = PlannedOp {
            from: "l.jpg".into(),
            ..op
        };
        assert_eq!(apply(&e.ctx(), &link).unwrap_err(), SkipReason::Symlink);
        assert!(e.path("b.jpg").exists());
    }

    /// A target folder that is a link, at any depth, is skipped and nothing leaves the
    /// library (#32); dry run agrees.
    #[cfg(unix)]
    #[test]
    fn a_linked_target_folder_is_skipped() {
        let e = Env::new();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), e.path("Archive")).unwrap();
        std::fs::create_dir(e.path("Photos")).unwrap();
        std::os::unix::fs::symlink(outside.path(), e.path("Photos/2026")).unwrap();
        for to_path in ["Archive/c.jpg", "Archive/new/c.jpg", "Photos/2026/05/c.jpg"] {
            let op = e.op("a.jpg", to(to_path));
            assert_eq!(
                check(&e.ctx(), &op, &Sim::default()),
                Err(SkipReason::Symlink),
                "{to_path}"
            );
            assert_eq!(
                apply(&e.ctx(), &op).unwrap_err(),
                SkipReason::Symlink,
                "{to_path}"
            );
        }
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
        assert!(e.path("a.jpg").exists() && e.path("a.jpg.xmp").exists());
        // Below a real folder it still goes ahead, making the folders it needs.
        apply(&e.ctx(), &e.op("a.jpg", to("Photos/2025/05/c.jpg"))).unwrap();
        assert!(e.path("Photos/2025/05/c.jpg").exists());
    }

    #[test]
    fn sim_sees_earlier_ops_of_the_batch() {
        let e = Env::new();
        let mut sim = Sim::default();
        sim.record("a.jpg", "x.jpg");
        let ctx = e.ctx();
        assert_eq!(
            check(&ctx, &e.op("b.jpg", to("x.jpg")), &sim),
            Err(SkipReason::TargetExists)
        );
        assert_eq!(check(&ctx, &e.op("b.jpg", to("a.jpg")), &sim), Ok(()));
        // Without the earlier op, a.jpg is taken.
        assert_eq!(
            check(&ctx, &e.op("b.jpg", to("a.jpg")), &Sim::default()),
            Err(SkipReason::TargetExists)
        );
    }

    fn crash(e: &Env, at: FailAt) {
        FAIL_AT.with(|f| f.set(Some(at)));
        let r = apply(&e.ctx(), &e.op("a.jpg", to("sub/c.jpg")));
        FAIL_AT.with(|f| f.set(None));
        assert_eq!(r.unwrap_err(), SkipReason::Io);
    }

    #[test]
    fn crash_after_media_leaves_the_sidecar_for_relink() {
        let e = Env::new();
        crash(&e, FailAt::AfterMedia);
        assert!(e.path("sub/c.jpg").exists() && !e.path("a.jpg").exists());
        assert!(e.path("a.jpg.xmp").exists() && !e.path("sub/c.jpg.xmp").exists());
        scan(&e.lib, &e.cache, &All).unwrap();
        crate::hashing::hash_pending(&e.lib, &e.cache, &Default::default()).unwrap();
        let journal = crate::journal::open_journal(
            crate::journal::JournalCtx {
                library: e.lib.clone(),
                cache: e.cache.clone(),
                ext: std::sync::Arc::new(All),
            },
            None,
        )
        .unwrap();
        relink_orphans(&e.lib, &e.cache, &journal).unwrap();
        assert!(e.path("sub/c.jpg.xmp").exists() && !e.path("a.jpg.xmp").exists());
    }

    /// The file moved, but nothing says so: only the undo of this one move is lost, and the
    /// next scan fixes the cache.
    #[test]
    fn crash_after_sidecar_leaves_a_stale_cache() {
        let e = Env::new();
        crash(&e, FailAt::AfterSidecar);
        assert!(e.path("sub/c.jpg").exists() && e.path("sub/c.jpg.xmp").exists());
        assert!(!e.path("a.jpg").exists() && !e.path("a.jpg.xmp").exists());
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_some());
        scan(&e.lib, &e.cache, &All).unwrap();
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_none());
        assert!(e.cache.get_by_path("sub/c.jpg").unwrap().is_some());
    }

    #[test]
    fn the_kept_hash_falls_back_to_a_valid_expect_hash_when_the_cached_one_is_stale() {
        let e = Env::new();
        let mut op = e.op("a.jpg", to("c.jpg"));
        let mk = |size, mtime_ns| StoredHash {
            hash: "h".into(),
            scope: "s".into(),
            file_hash: "f".into(),
            size,
            mtime_ns,
        };
        let mut rec = e.cache.get_by_path("a.jpg").unwrap().unwrap();
        rec.hash = Some(mk(op.expect.size + 1, op.expect.mtime_ns));
        e.cache.put(&rec).unwrap();
        let valid = mk(op.expect.size, op.expect.mtime_ns);
        op.expect.hash = Some(valid.clone());
        let done = apply(&e.ctx(), &op).unwrap();
        assert_eq!(done.hash, Some(valid));
    }

    #[test]
    fn a_sidecar_target_taken_mid_move_puts_the_media_back() {
        let e = Env::new();
        let sidecar_before = std::fs::read(e.path("a.jpg.xmp")).unwrap();
        FAIL_AT.with(|f| f.set(Some(FailAt::PlantTargetSidecar)));
        let r = apply(&e.ctx(), &e.op("a.jpg", to("c.jpg")));
        FAIL_AT.with(|f| f.set(None));
        assert_eq!(r.unwrap_err(), SkipReason::TargetExists);
        assert_eq!(std::fs::read(e.path("a.jpg")).unwrap(), b"photo a");
        assert_eq!(std::fs::read(e.path("a.jpg.xmp")).unwrap(), sidecar_before);
        assert_eq!(
            std::fs::read(e.path("c.jpg.xmp")).unwrap(),
            b"foreign sidecar"
        );
        assert!(!e.path("c.jpg").exists());
    }

    #[test]
    fn more_than_max_ops_is_refused() {
        let e = Env::new();
        let ops: Vec<OrganizeOp> = (0..=MAX_OPS)
            .map(|i| OrganizeOp {
                item_id: ItemId(format!("i{i}")),
                action: OrganizeAction::Trash,
                file_hash: None,
            })
            .collect();
        let err = plan_apply(&e.cache, &ops).err().unwrap();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert_eq!(err.message, "at most 10000 operations per batch");
    }

    #[test]
    fn cleanup_never_removes_the_root_meta_or_trash_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".asteria/trash/op1")).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        remove_emptied_folders(
            root,
            &[".asteria/trash/op1/x.jpg".into(), "a/b/y.jpg".into()],
        );
        assert!(!root.join(".asteria/trash/op1").exists());
        assert!(root.join(".asteria/trash").is_dir());
        assert!(!root.join("a").exists());
        assert!(root.is_dir());
    }
}
