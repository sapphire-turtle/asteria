//! The single per-library write queue. Every write to original files (sidecar edits,
//! orphan relinks, placing upload commits and organize batches) goes through one task that
//! runs jobs one at a time on the blocking pool, then refreshes the cache. An upload commit
//! hashes and stages its files before it joins the queue (see [`crate::upload_commit`]). An
//! organize job hands its batch to the Journal and waits for it to end, so the jobs behind it
//! wait too (§4.3).
//!
//! Shutdown: [`spawn_writer`] returns the [`Writer`] and the task's `JoinHandle`. Once
//! every `Writer` clone is dropped the task finishes the queued jobs and ends, so
//! `drop(writers); handle.await` drains the queue. When clones may outlive the shutdown
//! (a connection task still holding the app state), [`Writer::close`] refuses new jobs and
//! lets the task end after the queued ones.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asteria_common_api::{
    ItemId, MediaUpdateParams, OpResult, RatingOp, RejectReason, UploadCommitParams,
    UploadCommitResult,
};
use asteria_common_core::hash::{ContentHash, HashScope};
use asteria_common_core::journal::Actor;
use asteria_common_core::{Error as FwError, Library, UserId};
use chrono::{DateTime, Utc};
use corundum_framework::journal::{
    BatchId, BatchRecords, JournalError, OpKind as _, Origin, Planned as Op,
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cache::{Cache, ItemRecord, SidecarSnapshot, StoredHash};
use crate::error::ServerError;
use crate::ext::AppExtension;
use crate::hashing::{RelinkReport, relink_orphans};
use crate::journal::files::{self, OrganizeTidy, TidyMove, TidyPlan};
use crate::journal::map::actor_of;
use crate::journal::{ServerJournal, auto_label, wait_up_to};
use crate::organize::{self, Batch, OpCtx, Planned, Target};
use crate::scan::mtime_ns;
use crate::upload::{UploadError, UploadStore};
use crate::upload_commit::{Begun, Prepared, place, prepare_sealed, seal_group};

const QUEUE: usize = 256;
/// How long `organize` waits for a batch before it answers `running`; the batch goes on.
pub(crate) const APPLY_WAIT: Duration = Duration::from_secs(10);
/// How many upload commits hash and stage their files at once; more wait their turn, so
/// that big files do not all compete for the disk.
const COMMIT_PREPARE_JOBS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error("not found")]
    NotFound,
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("read-only: {0}")]
    ReadOnly(String),
    #[error("busy: {0}")]
    Busy(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// The Journal refused or failed the batch.
    #[error(transparent)]
    Journal(#[from] JournalError),
}

/// What became of one rating operation: its id, and either whether it changed the rating
/// with the item as it is now, or why it was refused.
pub type OpOutcome = (String, Result<(bool, ItemRecord), RejectReason>);

/// A piece of work for the queue: it gets the library and cache and reports on its own.
type ExecFn = Box<dyn FnOnce(&Arc<Library>, &Arc<Cache>) + Send>;

enum Job {
    Update {
        actor: Actor,
        params: MediaUpdateParams,
        reply: oneshot::Sender<Result<ItemRecord, WriteError>>,
    },
    Ops {
        actor: Actor,
        ops: Vec<RatingOp>,
        reply: oneshot::Sender<Result<Vec<OpOutcome>, WriteError>>,
    },
    Relink {
        reply: oneshot::Sender<Result<RelinkReport, WriteError>>,
    },
    /// The second phase of an upload commit; the first ran outside the queue.
    Place {
        store: Arc<UploadStore>,
        prepared: Prepared,
        reply: oneshot::Sender<Result<UploadCommitResult, UploadError>>,
    },
    /// Any other piece of work; it reports through a channel of its own.
    Exec(ExecFn),
    /// Refuse new jobs; finish the queued ones and end.
    Close,
}

/// How [`Writer::organize`] runs its operations.
pub(crate) enum BatchKind {
    Apply,
    /// Check everything, change nothing.
    DryRun,
}

#[derive(Clone)]
pub struct Writer {
    tx: mpsc::Sender<Job>,
    library: Arc<Library>,
    /// Runs and records the batches.
    journal: ServerJournal,
    /// Turns for the first phase of upload commits (`COMMIT_PREPARE_JOBS`).
    prepare_permits: Arc<Semaphore>,
    /// How long `organize` waits for a batch.
    organize_wait: Arc<Mutex<Duration>>,
}

/// Starts the writer task, which hands its batches to `journal`. Must be called inside a
/// Tokio runtime (it uses `tokio::spawn`). See the module docs for shutdown.
pub fn spawn_writer(
    library: Arc<Library>,
    cache: Arc<Cache>,
    journal: ServerJournal,
) -> (Writer, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Job>(QUEUE);
    let writer = Writer {
        tx,
        library: library.clone(),
        journal,
        prepare_permits: Arc::new(Semaphore::new(COMMIT_PREPARE_JOBS)),
        organize_wait: Arc::new(Mutex::new(APPLY_WAIT)),
    };
    let handle = tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let (lib, cache) = (library.clone(), cache.clone());
            match job {
                Job::Update {
                    actor,
                    params,
                    reply,
                } => {
                    let r = run_blocking(move || do_update(&lib, &cache, actor, params)).await;
                    let _ = reply.send(r);
                }
                Job::Ops { actor, ops, reply } => {
                    let r = run_blocking(move || do_ops(&lib, &cache, actor, ops)).await;
                    let _ = reply.send(r);
                }
                Job::Relink { reply } => {
                    let r = run_blocking(move || {
                        relink_orphans(&lib, &cache).map_err(|e| {
                            tracing::error!(error = %e, "relink failed");
                            WriteError::Internal(e.to_string())
                        })
                    })
                    .await;
                    let _ = reply.send(r);
                }
                Job::Place {
                    store,
                    prepared,
                    reply,
                } => {
                    let r = run_blocking(move || place(&lib, &cache, &store, prepared)).await;
                    let _ = reply.send(r);
                }
                Job::Exec(f) => {
                    let _ = run_blocking::<(), WriteError>(move || {
                        f(&lib, &cache);
                        Ok(())
                    })
                    .await;
                }
                Job::Close => rx.close(),
            }
        }
    });
    (writer, handle)
}

/// The error a job reports when its blocking task panicked.
trait JobError: Send + 'static {
    fn job_failed() -> Self;
}

impl JobError for WriteError {
    fn job_failed() -> Self {
        WriteError::Internal("write job failed".into())
    }
}

impl JobError for UploadError {
    fn job_failed() -> Self {
        UploadError::Io("write job failed".into())
    }
}

async fn run_blocking<T: Send + 'static, E: JobError>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, E> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "write job panicked");
            Err(E::job_failed())
        }
    }
}

fn stopped() -> WriteError {
    WriteError::Internal("writer stopped".into())
}

impl Writer {
    /// Applies `params` to an item's sidecar; the journal records `actor`, and the rating
    /// is that of the user `actor` acts as.
    pub async fn update(
        &self,
        actor: Actor,
        params: MediaUpdateParams,
    ) -> Result<ItemRecord, WriteError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Update {
                actor,
                params,
                reply,
            })
            .await
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// Applies rating operations in order, each as the rating `media.update` would set but
    /// at the operation's own time (never later than now), so the newest one wins. A
    /// refused operation (including one whose sidecar cannot be written) does not stop the
    /// others; only a failure of the whole call (the cache) fails it, and the client sends
    /// the whole batch again (a rating applied twice changes nothing).
    pub async fn apply_ops(
        &self,
        actor: Actor,
        ops: Vec<RatingOp>,
    ) -> Result<Vec<OpOutcome>, WriteError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Ops { actor, ops, reply })
            .await
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// Stops accepting jobs: later calls on any clone fail with "writer stopped", jobs
    /// queued before this one still run, then the writer task ends.
    pub async fn close(&self) {
        // An error means the writer has stopped already.
        let _ = self.tx.send(Job::Close).await;
    }

    /// Runs `f` as one job of the queue.
    async fn exec<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Arc<Library>, &Arc<Cache>) -> T + Send + 'static,
    ) -> Result<T, WriteError> {
        let (reply, rx) = oneshot::channel();
        let job = Box::new(move |lib: &Arc<Library>, cache: &Arc<Cache>| {
            let _ = reply.send(f(lib, cache));
        });
        self.tx.send(Job::Exec(job)).await.map_err(|_| stopped())?;
        // A dropped reply means the job panicked.
        rx.await
            .map_err(|_| WriteError::Internal("write job failed".into()))
    }

    /// Sets how long [`Writer::organize`] waits for a batch (tests use a short one).
    pub(crate) fn set_organize_wait(&self, wait: Duration) {
        *self.organize_wait.lock().unwrap_or_else(|p| p.into_inner()) = wait;
    }

    fn organize_wait(&self) -> Duration {
        *self.organize_wait.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs a batch of moves and trashes in the Journal, closed by an `organize_tidy`, as
    /// `actor` from `origin`; a batch without a label gets one. The operations skipped while
    /// planning are not run, and report under their request index. A dry run checks each
    /// operation on the queue and touches nothing. The operations update the cache, so the
    /// caller need not rescan.
    ///
    /// A batch that is not over after the wait (`set_organize_wait`) is answered with
    /// `running: true` and no results; it goes on, and `organize.status` reads its outcome
    /// from the Journal. A dry run is always waited for.
    pub(crate) async fn organize(
        &self,
        actor: Actor,
        origin: Origin,
        label: Option<String>,
        ops: Vec<Planned>,
        ext: Arc<dyn AppExtension>,
        kind: BatchKind,
    ) -> Result<Batch, WriteError> {
        if let BatchKind::DryRun = kind {
            return self.run_dry(ops, ext).await;
        }
        let deadline = Instant::now() + self.organize_wait();
        let mut indices = Vec::new();
        let mut early = Vec::new();
        let mut plans = Vec::new();
        let mut moves = Vec::new();
        for (i, planned) in ops.into_iter().enumerate() {
            match planned {
                Ok(op) => {
                    indices.push(i);
                    plans.push(files::planned(&op));
                    let to = match op.to {
                        Target::Path(to) => Some(to),
                        Target::Trash(_) => None,
                    };
                    moves.push(TidyMove { from: op.from, to });
                }
                Err((path, why)) => early.push(organize::skipped(i, &path, why)),
            }
        }
        let label = label.or_else(|| auto_label(files::Move::KIND, indices.len()));
        plans.push(Op::new::<OrganizeTidy>(&TidyPlan {
            user: organize::actor_user(actor),
            moves,
        }));
        let actor = actor_of(actor);
        let id = self
            .start_batch(move |j| j.submit(plans, &origin, &actor, label), |_, _| {})
            .await?;
        self.answer(id, deadline, move |b| {
            organize::merge_results(b, &indices, early)
        })
        .await
    }

    /// Undoes the batch `of` in the Journal, as `actor` from `origin`, and answers like
    /// [`Writer::organize`]. Once the undo has ended, the folders it emptied are removed. A
    /// dry run checks the moves back on the queue and touches nothing.
    ///
    /// A batch that cannot be undone (unknown, already undone or being undone, too old, or
    /// someone else's) is refused at once, before the queue.
    pub(crate) async fn undo(
        &self,
        of: BatchId,
        actor: Actor,
        origin: Origin,
        ext: Arc<dyn AppExtension>,
        dry_run: bool,
    ) -> Result<Batch, WriteError> {
        let deadline = Instant::now() + self.organize_wait();
        let actor = actor_of(actor);
        let (journal, checking) = (self.journal.clone(), actor.clone());
        let target = tokio::task::spawn_blocking(move || {
            journal.check_undo(of, &checking)?;
            journal.status(of)
        })
        .await
        .map_err(|e| WriteError::Internal(e.to_string()))??;
        if dry_run {
            return self.run_dry(organize::undo_plans(&target), ext).await;
        }
        let id = self
            .start_batch(
                move |j| j.undo_batch(of, &origin, &actor),
                |lib, b| organize::remove_emptied_folders(lib.root(), &organize::sources(b)),
            )
            .await?;
        self.answer(id, deadline, organize::results).await
    }

    /// Starts a batch with a job of the queue: `start` hands it to the Journal, then the job
    /// waits for it to end, so the jobs behind it wait too, and runs `ended` with it. Gives
    /// the batch's id as soon as it has started. (The Journal's operations never wait on the
    /// Journal themselves: that is done here.)
    async fn start_batch(
        &self,
        start: impl FnOnce(&ServerJournal) -> Result<BatchId, JournalError> + Send + 'static,
        ended: impl FnOnce(&Arc<Library>, &BatchRecords) + Send + 'static,
    ) -> Result<BatchId, WriteError> {
        let (started, rx) = oneshot::channel();
        let journal = self.journal.clone();
        let job = Box::new(move |lib: &Arc<Library>, _: &Arc<Cache>| {
            let id = match start(&journal) {
                Ok(id) => id,
                Err(e) => {
                    let _ = started.send(Err(e));
                    return;
                }
            };
            let _ = started.send(Ok(id));
            match journal.wait(id) {
                Ok(b) => ended(lib, &b),
                Err(e) => tracing::error!(error = %e, batch = %id, "a batch failed"),
            }
        });
        self.tx.send(Job::Exec(job)).await.map_err(|_| stopped())?;
        // A dropped sender means the job panicked.
        let started = rx
            .await
            .map_err(|_| WriteError::Internal("write job failed".into()))?;
        Ok(started?)
    }

    /// The answer for the batch `id`: its results by `report` if it ends by `deadline`,
    /// otherwise `running`.
    async fn answer(
        &self,
        id: BatchId,
        deadline: Instant,
        report: impl FnOnce(&BatchRecords) -> Vec<OpResult>,
    ) -> Result<Batch, WriteError> {
        let left = deadline.saturating_duration_since(Instant::now());
        Ok(match wait_up_to(&self.journal, id, left).await? {
            None => Batch {
                id: Some(id),
                results: Vec::new(),
                running: true,
            },
            Some(b) => Batch {
                id: Some(id),
                results: report(&b),
                running: false,
            },
        })
    }

    async fn run_dry(
        &self,
        ops: Vec<Planned>,
        ext: Arc<dyn AppExtension>,
    ) -> Result<Batch, WriteError> {
        let results = self
            .exec(move |lib, cache| {
                let ctx = OpCtx {
                    library: lib,
                    cache,
                    ext: ext.as_ref(),
                };
                organize::dry_run(&ctx, &ops)
            })
            .await?;
        Ok(Batch {
            id: None,
            results,
            running: false,
        })
    }

    pub async fn relink_orphans(&self) -> Result<RelinkReport, WriteError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Relink { reply })
            .await
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// Places a group of finished upload sessions under `Inbox/` (see
    /// [`crate::upload_commit`]). The sessions are checked, hashed and staged first, on the
    /// blocking pool but outside the queue (at most `COMMIT_PREPARE_JOBS` commits at once),
    /// so other writes go on meanwhile; only the renames wait their turn. Does not rescan;
    /// the caller asks for that.
    ///
    /// The commit runs in a task of its own: once started it finishes, and leaves its
    /// tombstones, even when the caller stops waiting (a client that timed out). A repeated
    /// commit meanwhile is `Busy` (the sessions are sealed), and afterwards gets the stored
    /// answer.
    pub async fn commit_upload(
        &self,
        store: Arc<UploadStore>,
        user: UserId,
        params: UploadCommitParams,
    ) -> Result<UploadCommitResult, UploadError> {
        let writer = self.clone();
        let task = tokio::spawn(async move { writer.run_commit(store, user, params).await });
        task.await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "upload commit task failed");
            Err(UploadError::job_failed())
        })
    }

    async fn run_commit(
        &self,
        store: Arc<UploadStore>,
        user: UserId,
        params: UploadCommitParams,
    ) -> Result<UploadCommitResult, UploadError> {
        let upload_stopped = || UploadError::Io("writer stopped".into());
        let sealing = store.clone();
        let sealed = match run_blocking(move || seal_group(&sealing, user, params)).await? {
            Begun::Answered(result) => return Ok(result),
            Begun::Sealed(sealed) => sealed,
        };
        let turn = self
            .prepare_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| upload_stopped())?;
        let (lib, staging) = (self.library.clone(), store.clone());
        let prepared: Prepared =
            run_blocking(move || prepare_sealed(&lib, &staging, sealed)).await?;
        drop(turn);
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Place {
                store,
                prepared,
                reply,
            })
            .await
            .map_err(|_| upload_stopped())?;
        rx.await.map_err(|_| upload_stopped())?
    }
}

fn map_fw(e: FwError) -> WriteError {
    match e {
        FwError::MediaNotFound(_) => WriteError::NotFound,
        FwError::InvalidPath(p) => WriteError::InvalidPath(p.display().to_string()),
        FwError::MalformedSidecar { reason, .. } => WriteError::ReadOnly(reason),
        e @ FwError::TmpExists(_) => WriteError::Busy(e.to_string()),
        e @ FwError::InvalidRating(_) => WriteError::Invalid(e.to_string()),
        e @ FwError::UnsafeSidecarWrite(_) => {
            tracing::error!(error = %e, "unsafe sidecar write refused");
            WriteError::Internal(e.to_string())
        }
        e => WriteError::Internal(e.to_string()),
    }
}

fn internal(e: ServerError) -> WriteError {
    tracing::error!(error = %e, "cache error during write");
    WriteError::Internal(e.to_string())
}

/// XML 1.0 cannot represent these characters.
fn xml_illegal(c: char) -> bool {
    (c < '\u{20}' && !matches!(c, '\t' | '\n' | '\r')) || matches!(c, '\u{FFFE}' | '\u{FFFF}')
}

fn validate(params: &MediaUpdateParams) -> Result<(), WriteError> {
    if let Some(r) = params.rating
        && !(-1..=5).contains(&r)
    {
        return Err(WriteError::Invalid(format!("rating out of range: {r}")));
    }
    let texts = params
        .subjects
        .iter()
        .chain(&params.hierarchical_subjects)
        .flatten()
        .map(String::as_str)
        .chain(params.description.iter().flatten().map(String::as_str))
        .chain(params.label.iter().flatten().map(String::as_str));
    for t in texts {
        if t.chars().any(xml_illegal) {
            return Err(WriteError::Invalid(
                "text contains characters not allowed in XML".into(),
            ));
        }
    }
    Ok(())
}

type Stamp = (u64, i128);

fn stat(path: &Path) -> Option<Stamp> {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), mtime_ns(&m)))
}

/// The cached hash, usable only if the file's current (size, mtime) equal the ones it was
/// computed for.
fn usable_hash(rec: &ItemRecord, now: Option<Stamp>) -> Option<(ContentHash, HashScope)> {
    let h: &StoredHash = rec.hash.as_ref()?;
    if now != Some((h.size, h.mtime_ns)) {
        return None;
    }
    Some((h.hash.parse().ok()?, h.scope.parse().ok()?))
}

/// The cached whole-file hash, under the same freshness rule as [`usable_hash`].
fn usable_file_hash(rec: &ItemRecord, now: Option<Stamp>) -> Option<ContentHash> {
    let h: &StoredHash = rec.hash.as_ref()?;
    if now != Some((h.size, h.mtime_ns)) {
        return None;
    }
    h.file_hash.parse().ok()
}

fn do_update(
    library: &Library,
    cache: &Cache,
    actor: Actor,
    params: MediaUpdateParams,
) -> Result<ItemRecord, WriteError> {
    validate(&params)?;
    let user = organize::actor_user(actor);
    let id: ItemId = params.id.clone();
    let rec = cache
        .get_by_id(&id)
        .map_err(internal)?
        .ok_or(WriteError::NotFound)?;
    if let Some(e) = &rec.sidecar_error {
        return Err(WriteError::ReadOnly(e.clone()));
    }
    let rel = Path::new(&rec.path);
    let stamp = stat(&library.root().join(rel));
    let hash = usable_hash(&rec, stamp);
    let file_hash = usable_file_hash(&rec, stamp);
    let owner = library.owner();

    // One closure: one sidecar write, one journal entry, no rehash when `hash` is set.
    let written = library.update_sidecar(rel, actor, |sc| {
        if sc.content_hash.is_none() {
            sc.content_hash = hash;
        }
        sc.file_hash = file_hash;
        if let Some(v) = params.subjects {
            sc.subjects = v.into_iter().collect::<BTreeSet<_>>();
        }
        if let Some(v) = params.hierarchical_subjects {
            sc.hierarchical_subjects = v.into_iter().collect::<BTreeSet<_>>();
        }
        if let Some(v) = params.description {
            sc.description = v;
        }
        if let Some(v) = params.label {
            sc.label = v;
        }
        if let Some(value) = params.rating {
            // Range was validated above.
            let _ = sc.apply_rating(owner, user, value, Utc::now());
        }
    });
    let written = match written {
        Err(e @ (FwError::MediaNotFound(_) | FwError::InvalidPath(_))) => return Err(map_fw(e)),
        other => other,
    };
    // Refresh from disk after any write attempt, best effort.
    if let Err(e) = refresh_cache(library, cache, &rec) {
        tracing::warn!(error = %e, path = %rec.path, "cache refresh after write failed");
    }
    written.map_err(map_fw)?;
    cache
        .get_by_id(&id)
        .map_err(internal)?
        .ok_or(WriteError::NotFound)
}

fn do_ops(
    library: &Library,
    cache: &Cache,
    actor: Actor,
    ops: Vec<RatingOp>,
) -> Result<Vec<OpOutcome>, WriteError> {
    let user = organize::actor_user(actor);
    let owner = library.owner();
    let now = Utc::now();
    ops.into_iter()
        .map(|op| {
            let outcome = match parse_rating_op(&op, now) {
                Ok(at) => do_rating(library, cache, actor, owner, user, &op.item, op.value, at),
                Err(reason) => Ok(Err(reason)),
            };
            outcome.map(|o| (op.id, o))
        })
        .collect()
}

/// The time of `op`, held back to `now`; or why the operation is invalid.
fn parse_rating_op(op: &RatingOp, now: DateTime<Utc>) -> Result<DateTime<Utc>, RejectReason> {
    if !(-1..=5).contains(&op.value) {
        return Err(RejectReason::Invalid);
    }
    let at = DateTime::parse_from_rfc3339(&op.at).map_err(|_| RejectReason::Invalid)?;
    Ok(at.with_timezone(&Utc).min(now))
}

/// One rating, written like [`do_update`] writes one: a single sidecar write, hash fields
/// set the same way. The outer error (the cache failed) fails the whole call; the inner one
/// refuses this operation, as does a sidecar that cannot be written.
#[allow(clippy::too_many_arguments)]
fn do_rating(
    library: &Library,
    cache: &Cache,
    actor: Actor,
    owner: UserId,
    user: UserId,
    id: &ItemId,
    value: i8,
    at: DateTime<Utc>,
) -> Result<Result<(bool, ItemRecord), RejectReason>, WriteError> {
    let Some(rec) = cache.get_by_id(id).map_err(internal)? else {
        return Ok(Err(RejectReason::NotFound));
    };
    if rec.sidecar_error.is_some() {
        return Ok(Err(RejectReason::ReadOnly));
    }
    let rel = Path::new(&rec.path);
    let stamp = stat(&library.root().join(rel));
    let hash = usable_hash(&rec, stamp);
    let file_hash = usable_file_hash(&rec, stamp);
    let mut changed = false;
    let written = library.update_sidecar(rel, actor, |sc| {
        if sc.content_hash.is_none() {
            sc.content_hash = hash;
        }
        sc.file_hash = file_hash;
        // The value's range was checked before.
        changed = sc.apply_rating(owner, user, value, at).unwrap_or(false);
    });
    if let Err(FwError::MediaNotFound(_)) = written {
        return Ok(Err(RejectReason::NotFound));
    }
    if let Err(e) = refresh_cache(library, cache, &rec) {
        tracing::warn!(error = %e, path = %rec.path, "cache refresh after write failed");
    }
    if let Err(e) = written {
        // This item's sidecar cannot be written (broken since the scan, or an I/O error):
        // the operation is refused, and the others still go.
        match e {
            FwError::UnsafeSidecarWrite(_) => {
                tracing::error!(error = %e, path = %rec.path, "unsafe sidecar write refused");
            }
            _ => tracing::warn!(error = %e, path = %rec.path, "cannot write a rating"),
        }
        return Ok(Err(RejectReason::ReadOnly));
    }
    match cache.get_by_id(id).map_err(internal)? {
        Some(rec) => Ok(Ok((changed, rec))),
        None => Ok(Err(RejectReason::NotFound)),
    }
}

fn refresh_cache(library: &Library, cache: &Cache, rec: &ItemRecord) -> Result<(), ServerError> {
    let rel = Path::new(&rec.path);
    // The cache's StoredHash is filled only by hash_pending, never from the sidecar.
    match library.read_sidecar(rel) {
        Ok(Some(sc)) => {
            cache.set_sidecar(&rec.path, SidecarSnapshot::from(&sc), None)?;
        }
        Ok(None) => {
            cache.set_sidecar(&rec.path, SidecarSnapshot::default(), None)?;
        }
        Err(FwError::MalformedSidecar { reason, .. }) => {
            cache.set_sidecar(&rec.path, rec.sidecar.clone(), Some(reason))?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
