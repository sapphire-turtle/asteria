//! Moving, renaming and trashing one media file together with its sidecar: the per-operation
//! core of the organize batches, and the pieces the writer strings into a batch (request
//! validation, dry runs, the closing steps).
//!
//! Writes are ordered original, journal, cache. A move never replaces an existing file, and
//! a stop at any point never loses an original: after the media moved but before the sidecar
//! did, `relink_orphans` finds the sidecar again; a missing journal line only loses the
//! undo of that one move; a stale cache is fixed by the next scan.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::Path;

use asteria_framework::fsutil::atomic_write;
use asteria_framework::hash::file_hash;
use asteria_framework::journal::{Actor, BatchId, JournalEntry, OpId, OpKind};
use asteria_framework::paths::{META_DIR, TRASH_DIR, sidecar_path_for, trash_rel};
use asteria_framework::upload_path::validate_inbox_path;
use asteria_framework::{Error as FwError, Library, classify};
use asteria_framework_api::{
    BatchSummary, ItemId, MAX_LABEL_CHARS, MAX_OPS, OpResult, OpStatus, OrganizeAction,
    OrganizeHistory, OrganizeOp, RpcError, SkipReason, codes,
};
use chrono::{SecondsFormat, Utc};
use grain_id::GrainId;

use crate::cache::{Cache, ExtraRecord, ItemRecord, SidecarSnapshot, StoredHash, item_id_for};
use crate::ext::{AppExtension, ExtContext, Rewrite};
use crate::fsutil::move_no_clobber;
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
    Trash,
}

/// What the file looked like when the operation was planned.
#[derive(Debug, Clone)]
pub(crate) struct Expect {
    pub size: u64,
    pub mtime_ns: i128,
    /// Whole-file hash (`file/1`); compared when given.
    pub file_hash: Option<String>,
    /// Brought back into the cache when a file is restored from the trash; never compared.
    pub hash: Option<StoredHash>,
}

#[derive(Debug, Clone)]
pub(crate) struct Done {
    /// Where the file went; the trash path for a trashed file.
    pub to: String,
    /// The new record, when the file is now somewhere the library shows.
    pub record: Option<ItemRecord>,
}

pub(crate) struct OpCtx<'a> {
    pub library: &'a Library,
    pub cache: &'a Cache,
    pub ext: &'a dyn AppExtension,
    pub actor: Actor,
    pub batch: Option<BatchId>,
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
    AfterJournal,
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
        let blocked = sim.taken.contains(to)
            || (!sim.freed.contains(to)
                && (exists(&root.join(to)).map_err(|e| io_skip("cannot stat target", to, &e))?
                    || exists(&sidecar_path_for(&root.join(to)))
                        .map_err(|e| io_skip("cannot stat target sidecar", to, &e))?));
        if blocked {
            return Err(SkipReason::TargetExists);
        }
    }
    Ok(())
}

fn exists(p: &Path) -> std::io::Result<bool> {
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

/// Carries `op` out: the media file and its sidecar move, the move is journaled, and the cache
/// follows. Checks again against the disk first; never replaces an existing file.
pub(crate) fn apply(ctx: &OpCtx, op: &PlannedOp) -> Result<Done, SkipReason> {
    check(ctx, op, &Sim::default())?;
    let root = ctx.library.root();
    let op_id = OpId(GrainId::random());
    let to = match &op.to {
        Target::Path(p) => p.clone(),
        Target::Trash => trash_rel(&op_id, &op.from),
    };
    let from_abs = root.join(&op.from);
    let to_abs = root.join(&to);
    let from_sidecar = sidecar_path_for(&from_abs);
    let to_sidecar = sidecar_path_for(&to_abs);
    let has_sidecar =
        exists(&from_sidecar).map_err(|e| io_skip("cannot stat sidecar", &op.from, &e))?;

    if let Some(parent) = to_abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_skip("cannot create folder", &to, &e))?;
    }
    move_no_clobber(&from_abs, &to_abs).map_err(|e| match e.kind() {
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
    if has_sidecar && let Err(e) = move_no_clobber(&from_sidecar, &to_sidecar) {
        // Never leave the media moved with its sidecar behind.
        if let Err(back) = move_no_clobber(&to_abs, &from_abs) {
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
    let kind = if in_meta_dir(&to) {
        OpKind::Trash
    } else if op.from.rsplit_once('/').map(|p| p.0) == to.rsplit_once('/').map(|p| p.0) {
        OpKind::Rename
    } else {
        OpKind::Move
    };
    let entry = JournalEntry {
        op_id,
        plan_id: ctx.batch,
        actor: ctx.actor,
        at: Utc::now(),
        kind,
        before: serde_json::json!({
            "path": op.from,
            "size": op.expect.size,
            "mtime_ns": op.expect.mtime_ns as i64,
            "hash": hash,
        }),
        after: serde_json::json!({ "path": to, "sidecar": has_sidecar }),
    };
    // The original has moved, so a journal failure cannot undo that: log it and go on.
    if let Err(e) = ctx.library.journal().and_then(|j| j.append(&entry)) {
        tracing::error!(from = %op.from, %to, error = %e, "moved a file but could not journal it");
    }
    #[cfg(test)]
    stop_at(FailAt::AfterJournal)?;

    let record = update_cache(ctx, op, &to);
    Ok(Done { to, record })
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

/// What a finished batch reports.
pub struct Batch {
    /// `None` for a dry run.
    pub id: Option<BatchId>,
    pub results: Vec<OpResult>,
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
    let mut seen_targets: std::collections::HashMap<&str, usize> = Default::default();
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
                if let Some(j) = seen_targets.insert(to.as_str(), i) {
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
            OrganizeAction::Trash => Target::Trash,
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

/// The report of an operation that `apply` carried out.
pub(crate) fn applied(index: usize, op: &PlannedOp, d: Done) -> OpResult {
    match op.to {
        Target::Path(_) => done(index, &op.from, Some(d.to), d.record.map(|r| r.id)),
        Target::Trash => done(index, &op.from, None, None),
    }
}

/// Checks every operation in order without touching anything; each one sees the effect the
/// earlier ones would have.
pub(crate) fn dry_run(ctx: &OpCtx, ops: &[Planned]) -> Vec<OpResult> {
    let mut sim = Sim::default();
    let lib_id = &ctx.library.config().id;
    ops.iter()
        .enumerate()
        .map(|(i, planned)| match planned {
            Err((path, why)) => skipped(i, path, *why),
            Ok(op) => match check(ctx, op, &sim) {
                Err(why) => skipped(i, &op.from, why),
                Ok(()) => match &op.to {
                    Target::Path(to) => {
                        sim.record(&op.from, to);
                        done(i, &op.from, Some(to.clone()), Some(item_id_for(lib_id, to)))
                    }
                    Target::Trash => {
                        sim.free(&op.from);
                        done(i, &op.from, None, None)
                    }
                },
            },
        })
        .collect()
}

/// The user an actor acts for.
pub(crate) fn actor_user(actor: Actor) -> asteria_framework::UserId {
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

/// The first journal line of a batch.
pub(crate) fn write_header(
    ctx: &OpCtx,
    label: Option<&str>,
    op_count: usize,
    undo_of: Option<BatchId>,
) -> Result<(), FwError> {
    append(
        ctx,
        OpKind::Batch,
        serde_json::Value::Null,
        serde_json::json!({ "label": label, "op_count": op_count, "undo_of": undo_of }),
    )
}

fn append(
    ctx: &OpCtx,
    kind: OpKind,
    before: serde_json::Value,
    after: serde_json::Value,
) -> Result<(), FwError> {
    ctx.library.journal()?.append(&JournalEntry {
        op_id: OpId(GrainId::random()),
        plan_id: ctx.batch,
        actor: ctx.actor,
        at: Utc::now(),
        kind,
        before,
        after,
    })
}

/// The last write job of a batch: removes the folders the moves emptied, applies the
/// rewrites and journals the end.
pub(crate) fn finish(
    ctx: &OpCtx,
    sources: &[String],
    rewrites: &[Rewrite],
    done: usize,
    skipped: usize,
) {
    remove_emptied_folders(ctx.library.root(), sources);
    for rw in rewrites {
        apply_rewrite(ctx, rw);
    }
    let end = append(
        ctx,
        OpKind::BatchEnd,
        serde_json::Value::Null,
        serde_json::json!({ "done": done, "skipped": skipped }),
    );
    if let Err(e) = end {
        tracing::error!(error = %e, "could not journal the end of a batch");
    }
}

/// Removes the folders that `sources` left empty, up towards the root. The library root,
/// `.asteria` and the trash folder stay.
fn remove_emptied_folders(root: &Path, sources: &[String]) {
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

fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !in_meta_dir(rel)
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Writes `rw.after` over `rw.path` if the file still holds `rw.before`, journals it and
/// updates the extras record. Anything else is logged and left alone.
fn apply_rewrite(ctx: &OpCtx, rw: &Rewrite) {
    if !safe_rel(&rw.path) {
        tracing::warn!(path = %rw.path, "rewrite refused: not a library path");
        return;
    }
    let abs = ctx.library.root().join(&rw.path);
    match std::fs::read_to_string(&abs) {
        Ok(now) if now == rw.before => {}
        Ok(_) => {
            tracing::warn!(path = %rw.path, "rewrite skipped: the file changed meanwhile");
            return;
        }
        Err(e) => {
            tracing::warn!(path = %rw.path, error = %e, "rewrite skipped: cannot read the file");
            return;
        }
    }
    if let Err(e) = atomic_write(&abs, rw.after.as_bytes()) {
        tracing::warn!(path = %rw.path, error = %e, "rewrite failed");
        return;
    }
    let journaled = append(
        ctx,
        OpKind::RewritePlaylist,
        serde_json::json!({ "path": rw.path, "content": rw.before }),
        serde_json::json!({ "path": rw.path, "content": rw.after }),
    );
    if let Err(e) = journaled {
        tracing::error!(path = %rw.path, error = %e, "rewrote a file but could not journal it");
    }
    if ctx.ext.wants_extra(&rw.path)
        && let Ok(meta) = std::fs::metadata(&abs)
    {
        let rec = ExtraRecord {
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
        };
        if let Err(e) = ctx.cache.put_extras(&[(rw.path.clone(), rec)]) {
            tracing::warn!(path = %rw.path, error = %e, "extras not updated; the next scan will fix it");
        }
    }
}

/// What `organize.undo` runs: the inverse operations, newest first, and the playlists to put
/// back afterwards.
pub(crate) struct UndoPlan {
    pub ops: Vec<PlannedOp>,
    pub restores: Vec<Rewrite>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UndoError {
    NotFound,
    AlreadyUndone,
}

fn header_undo_of(e: &JournalEntry) -> Option<BatchId> {
    serde_json::from_value(e.after.get("undo_of")?.clone()).ok()
}

/// The batches in the journal, newest first.
pub(crate) fn history(entries: &[JournalEntry]) -> Vec<BatchSummary> {
    let mut order: Vec<BatchSummary> = Vec::new();
    let mut index: HashMap<BatchId, usize> = HashMap::new();
    for e in entries {
        let Some(id) = e.plan_id else { continue };
        match e.kind {
            OpKind::Batch if !index.contains_key(&id) => {
                let (user, agent) = match e.actor {
                    Actor::User(u) => (u, false),
                    Actor::Agent { on_behalf_of } => (on_behalf_of, true),
                };
                index.insert(id, order.len());
                order.push(BatchSummary {
                    batch_id: id.0.to_string(),
                    user: user.to_string(),
                    agent,
                    at: e.at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
                    label: parsed(&e.after, "label"),
                    op_count: parsed(&e.after, "op_count").unwrap_or(0),
                    done: 0,
                    skipped: 0,
                    complete: false,
                    undo_of: header_undo_of(e).map(|b| b.0.to_string()),
                    undone_by: None,
                });
            }
            OpKind::Move | OpKind::Rename | OpKind::Trash => {
                if let Some(&i) = index.get(&id) {
                    order[i].done += 1;
                }
            }
            OpKind::BatchEnd => {
                if let Some(&i) = index.get(&id) {
                    order[i].complete = true;
                    order[i].skipped = parsed(&e.after, "skipped").unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    // In journal order, so the newest undo of a batch is the one that stays.
    for i in 0..order.len() {
        let Some(target) = order[i].undo_of.clone() else {
            continue;
        };
        let by = order[i].batch_id.clone();
        if let Some(&t) = target
            .parse::<GrainId>()
            .ok()
            .and_then(|g| index.get(&BatchId(g)))
        {
            order[t].undone_by = Some(by);
        }
    }
    order.reverse();
    order
}

/// One page of `all` (newest first): the batches older than `cursor`, at most `limit`.
pub(crate) fn history_page(
    all: Vec<BatchSummary>,
    limit: u32,
    cursor: Option<&str>,
) -> Result<OrganizeHistory, RpcError> {
    let start = match cursor {
        None => 0,
        Some(c) => {
            all.iter()
                .position(|b| b.batch_id == c)
                .ok_or_else(|| invalid("unknown cursor"))?
                + 1
        }
    };
    let mut batches: Vec<BatchSummary> = all.into_iter().skip(start).collect();
    let more = batches.len() > limit as usize;
    batches.truncate(limit as usize);
    let next_cursor = if more {
        batches.last().map(|b| b.batch_id.clone())
    } else {
        None
    };
    Ok(OrganizeHistory {
        batches,
        next_cursor,
    })
}

fn parsed<T: serde::de::DeserializeOwned>(v: &serde_json::Value, key: &str) -> Option<T> {
    serde_json::from_value(v.get(key)?.clone()).ok()
}

/// A path as the journal wrote it: relative, with plain components only (it may lie in the
/// trash folder).
fn plain_rel(rel: &str) -> bool {
    !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The inverse of batch `id`. Fails when there is no such batch or an undo of it exists.
/// A line that cannot be read is logged and left out.
pub(crate) fn plan_undo(entries: &[JournalEntry], id: BatchId) -> Result<UndoPlan, UndoError> {
    let mut found = false;
    let mut ops = Vec::new();
    let mut restores = Vec::new();
    for e in entries {
        if e.kind == OpKind::Batch && header_undo_of(e) == Some(id) {
            return Err(UndoError::AlreadyUndone);
        }
        if e.plan_id != Some(id) {
            continue;
        }
        match e.kind {
            OpKind::Batch => found = true,
            OpKind::Move | OpKind::Rename | OpKind::Trash => match inverse(e) {
                Some(op) => ops.push(op),
                None => tracing::warn!(op = %e.op_id.0, "journal line cannot be undone"),
            },
            OpKind::RewritePlaylist => match inverse_rewrite(e) {
                Some(rw) => restores.push(rw),
                None => tracing::warn!(op = %e.op_id.0, "journal line cannot be undone"),
            },
            _ => {}
        }
    }
    if !found {
        return Err(UndoError::NotFound);
    }
    ops.reverse();
    restores.reverse();
    Ok(UndoPlan { ops, restores })
}

fn inverse(e: &JournalEntry) -> Option<PlannedOp> {
    let to: String = parsed(&e.before, "path").filter(|p: &String| plain_rel(p))?;
    let from: String = parsed(&e.after, "path").filter(|p: &String| plain_rel(p))?;
    let mtime: i64 = parsed(&e.before, "mtime_ns")?;
    Some(PlannedOp {
        from,
        to: Target::Path(to),
        expect: Expect {
            size: parsed(&e.before, "size")?,
            mtime_ns: i128::from(mtime),
            file_hash: None,
            hash: parsed(&e.before, "hash"),
        },
    })
}

fn inverse_rewrite(e: &JournalEntry) -> Option<Rewrite> {
    Some(Rewrite {
        path: parsed(&e.after, "path").filter(|p: &String| plain_rel(p))?,
        before: parsed(&e.after, "content")?,
        after: parsed(&e.before, "content")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::relink_orphans;
    use crate::scan::scan;
    use asteria_framework::MediaKind;

    struct All;
    impl AppExtension for All {
        fn accepts(&self, _: MediaKind) -> bool {
            true
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        lib: Library,
        cache: Cache,
        batch: BatchId,
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
            lib.update_sidecar(Path::new("a.jpg"), Actor::User(lib.owner()), |sc| {
                sc.subjects.insert("x".into());
            })
            .unwrap();
            scan(&lib, &cache, &All).unwrap();
            Env {
                _dir: dir,
                lib,
                cache,
                batch: BatchId(GrainId::random()),
            }
        }

        fn ctx(&self) -> OpCtx<'_> {
            OpCtx {
                library: &self.lib,
                cache: &self.cache,
                ext: &All,
                actor: Actor::User(self.lib.owner()),
                batch: Some(self.batch),
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

        fn last(&self) -> JournalEntry {
            self.lib
                .journal()
                .unwrap()
                .read_all()
                .unwrap()
                .pop()
                .unwrap()
        }

        fn journal_len(&self) -> usize {
            self.lib.journal().unwrap().read_all().unwrap().len()
        }
    }

    fn to(p: &str) -> Target {
        Target::Path(p.into())
    }

    #[test]
    fn moves_media_and_sidecar_and_journals_a_move() {
        let e = Env::new();
        let done = apply(&e.ctx(), &e.op("a.jpg", to("sub/c.jpg"))).unwrap();
        assert_eq!(done.to, "sub/c.jpg");
        assert_eq!(done.record.unwrap().path, "sub/c.jpg");
        assert!(!e.path("a.jpg").exists() && !e.path("a.jpg.xmp").exists());
        assert!(e.path("sub/c.jpg").exists() && e.path("sub/c.jpg.xmp").exists());
        let j = e.last();
        assert_eq!(j.kind, OpKind::Move);
        assert_eq!(j.plan_id, Some(e.batch));
        assert_eq!(j.before["path"], "a.jpg");
        assert_eq!(j.after["path"], "sub/c.jpg");
        assert_eq!(j.after["sidecar"], true);
        assert!(e.cache.get_by_path("sub/c.jpg").unwrap().is_some());
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_none());
    }

    #[test]
    fn same_folder_is_a_rename() {
        let e = Env::new();
        let before = e.cache.revision();
        apply(&e.ctx(), &e.op("a.jpg", to("c.jpg"))).unwrap();
        assert_eq!(e.last().kind, OpKind::Rename);
        assert!(e.cache.revision() > before);
    }

    #[test]
    fn trash_goes_under_the_op_id_and_leaves_the_cache() {
        let e = Env::new();
        let done = apply(&e.ctx(), &e.op("a.jpg", Target::Trash)).unwrap();
        let j = e.last();
        assert_eq!(j.kind, OpKind::Trash);
        assert_eq!(done.to, format!(".asteria/trash/{}/a.jpg", j.op_id.0));
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
        let done = apply(&e.ctx(), &e.op("a.jpg", Target::Trash)).unwrap();
        let stored: StoredHash = serde_json::from_value(e.last().before["hash"].clone()).unwrap();
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
        assert_eq!(e.journal_len(), 1); // only the sidecar update of setup
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
        let n = e.journal_len();
        crash(&e, FailAt::AfterMedia);
        assert!(e.path("sub/c.jpg").exists() && !e.path("a.jpg").exists());
        assert!(e.path("a.jpg.xmp").exists() && !e.path("sub/c.jpg.xmp").exists());
        assert_eq!(e.journal_len(), n);
        scan(&e.lib, &e.cache, &All).unwrap();
        crate::hashing::hash_pending(&e.lib, &e.cache, &Default::default()).unwrap();
        relink_orphans(&e.lib, &e.cache).unwrap();
        assert!(e.path("sub/c.jpg.xmp").exists() && !e.path("a.jpg.xmp").exists());
    }

    #[test]
    fn crash_after_sidecar_loses_only_the_journal_line() {
        let e = Env::new();
        let n = e.journal_len();
        crash(&e, FailAt::AfterSidecar);
        assert!(e.path("sub/c.jpg").exists() && e.path("sub/c.jpg.xmp").exists());
        assert!(!e.path("a.jpg").exists() && !e.path("a.jpg.xmp").exists());
        assert_eq!(e.journal_len(), n);
    }

    #[test]
    fn crash_after_journal_is_recorded() {
        let e = Env::new();
        let n = e.journal_len();
        crash(&e, FailAt::AfterJournal);
        assert_eq!(e.journal_len(), n + 1);
        assert_eq!(e.last().kind, OpKind::Move);
        // The cache is stale until the next scan.
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_some());
        scan(&e.lib, &e.cache, &All).unwrap();
        assert!(e.cache.get_by_path("a.jpg").unwrap().is_none());
        assert!(e.cache.get_by_path("sub/c.jpg").unwrap().is_some());
    }

    #[test]
    fn journal_hash_falls_back_to_a_valid_expect_hash_when_the_cached_one_is_stale() {
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
        apply(&e.ctx(), &op).unwrap();
        let stored: StoredHash = serde_json::from_value(e.last().before["hash"].clone()).unwrap();
        assert_eq!(stored, valid);
    }

    #[test]
    fn a_sidecar_target_taken_mid_move_puts_the_media_back() {
        let e = Env::new();
        let n = e.journal_len();
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
        assert_eq!(e.journal_len(), n);
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
