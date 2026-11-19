//! The single per-library write queue. Every write to original files (sidecar edits,
//! orphan relinks, placing upload commits and organize batches) goes through one task that
//! runs jobs one at a time on the blocking pool, then refreshes the cache. An upload commit
//! hashes and stages its files before it joins the queue (see [`crate::upload_commit`]). Each
//! job hands its batch to the Journal and waits for it to end, so the jobs behind it wait too
//! (§4.3).
//!
//! Shutdown: [`spawn_writer`] returns the [`Writer`] and the task's `JoinHandle`. Once
//! every `Writer` clone is dropped the task finishes the queued jobs and ends, so
//! `drop(writers); handle.await` drains the queue. When clones may outlive the shutdown
//! (a connection task still holding the app state), [`Writer::close`] refuses new jobs and
//! lets the task end after the queued ones.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asteria_common_api::{
    MediaUpdateParams, OpResult, RatingOp, RejectReason, UploadCommitParams, UploadCommitResult,
};
use asteria_common_core::Actor;
use asteria_common_core::{Library, UserId};
use chrono::{DateTime, Utc};
use corundum_framework::journal::{
    BatchId, BatchRecords, JournalError, OpKind as _, Origin, Outcome, Planned as Op, Skip,
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cache::{Cache, ItemRecord};
use crate::error::ServerError;
use crate::ext::AppExtension;
use crate::hashing::{RelinkReport, relink_orphans};
use crate::journal::files::{self, EarlySkip, OrganizeTidy, TidyMove, TidyPlan};
use crate::journal::map::actor_of;
use crate::journal::sidecar::{
    Fields, Rate, RateDone, RatePlan, RatingAt, WriteSidecar, WriteSidecarPlan,
};
use crate::journal::{ServerJournal, auto_label, wait_up_to};
use crate::organize::{self, Batch, OpCtx, Planned, Target};
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
    Relink {
        reply: oneshot::Sender<Result<RelinkReport, WriteError>>,
    },
    /// The second phase of an upload commit; the first ran outside the queue.
    Place {
        store: Arc<UploadStore>,
        /// Boxed: it is much larger than the other jobs.
        prepared: Box<Prepared>,
        origin: Origin,
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
    cache: Arc<Cache>,
    /// Runs and records the batches.
    journal: ServerJournal,
    /// Turns for the first phase of upload commits (`COMMIT_PREPARE_JOBS`).
    prepare_permits: Arc<Semaphore>,
    /// How long a batch may take to start (and `organize` waits for it to end).
    write_wait: Arc<Mutex<Duration>>,
}

/// Starts the writer task, which hands its batches to `journal`. Must be called inside a
/// Tokio runtime (it uses `tokio::spawn`). See the module docs for shutdown.
pub fn spawn_writer(
    library: Arc<Library>,
    cache: Arc<Cache>,
    journal: ServerJournal,
) -> (Writer, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Job>(QUEUE);
    let queue_journal = journal.clone();
    let writer = Writer {
        tx,
        library: library.clone(),
        cache: cache.clone(),
        journal,
        prepare_permits: Arc::new(Semaphore::new(COMMIT_PREPARE_JOBS)),
        write_wait: Arc::new(Mutex::new(APPLY_WAIT)),
    };
    let handle = tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let (lib, cache, journal) = (library.clone(), cache.clone(), queue_journal.clone());
            match job {
                Job::Relink { reply } => {
                    let r = run_blocking(move || {
                        relink_orphans(&lib, &cache, &journal).map_err(|e| {
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
                    origin,
                    reply,
                } => {
                    let r = run_blocking(move || {
                        place(&lib, &cache, &journal, &store, &origin, *prepared)
                    })
                    .await;
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

/// Who has a queued batch job: nobody yet, the queue (it runs), or the caller (it never
/// runs). The first to take it has it.
#[derive(Default)]
struct Claim(std::sync::atomic::AtomicU8);

impl Claim {
    const RUN: u8 = 1;
    const DROP: u8 = 2;

    fn take(&self, by: u8) -> bool {
        use std::sync::atomic::Ordering::AcqRel;
        self.0
            .compare_exchange(0, by, AcqRel, std::sync::atomic::Ordering::Acquire)
            .is_ok()
    }
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
    /// Applies `params` to an item's sidecar as one batch, recorded as `actor` from `origin`:
    /// a `write_sidecar` for the tags, description and label, then a `rate` for the rating
    /// (that of the user `actor` acts as). A batch the queue has not started within the wait
    /// (`set_write_wait`) is `Busy` and never runs (§4.3); once started, it is waited for.
    pub async fn update(
        &self,
        actor: Actor,
        origin: Origin,
        params: MediaUpdateParams,
    ) -> Result<ItemRecord, WriteError> {
        validate(&params)?;
        let deadline = Instant::now() + self.write_wait();
        let user = organize::actor_user(actor);
        let at_ms = Utc::now().timestamp_millis();
        let (rec, b) = self
            .run_batch(actor, origin, deadline, move |cache| {
                let rec = cache
                    .get_by_id(&params.id)
                    .map_err(internal)?
                    .ok_or(WriteError::NotFound)?;
                if let Some(e) = &rec.sidecar_error {
                    return Err(WriteError::ReadOnly(e.clone()));
                }
                let mut plans = Vec::new();
                let set = Fields {
                    subjects: params.subjects.map(|v| v.into_iter().collect()),
                    hierarchical_subjects: params
                        .hierarchical_subjects
                        .map(|v| v.into_iter().collect()),
                    description: params.description,
                    label: params.label,
                };
                if !set.is_empty() {
                    plans.push(Op::new::<WriteSidecar>(&WriteSidecarPlan {
                        path: rec.path.clone(),
                        set,
                        expect: None,
                    }));
                }
                if let Some(value) = params.rating {
                    plans.push(Op::new::<Rate>(&RatePlan {
                        path: rec.path.clone(),
                        user,
                        rating: Some(RatingAt {
                            value,
                            at_ms: Some(at_ms),
                        }),
                        expect: None,
                    }));
                }
                Ok((rec, plans))
            })
            .await?;
        let skipped = b
            .iter()
            .flat_map(|b| &b.ops)
            .find_map(|o| match &o.outcome {
                Outcome::Skipped(skip) => Some(skip.clone()),
                Outcome::Done(_) => None,
            });
        let cache = self.cache.clone();
        blocking(move || {
            if let Some(skip) = skipped {
                return Err(match skip {
                    Skip::NotFound => WriteError::NotFound,
                    Skip::InvalidPath => WriteError::InvalidPath(rec.path),
                    // The reason the cache has now: never a server path.
                    Skip::Denied => WriteError::ReadOnly(
                        cache
                            .get_by_path(&rec.path)
                            .map_err(internal)?
                            .and_then(|r| r.sidecar_error)
                            .unwrap_or_else(|| "malformed sidecar".into()),
                    ),
                    Skip::Exists => WriteError::Busy("a sidecar write is under way".into()),
                    Skip::InvalidPlan(m) => WriteError::Invalid(m),
                    other => WriteError::Internal(other.to_string()),
                });
            }
            cache
                .get_by_id(&rec.id)
                .map_err(internal)?
                .ok_or(WriteError::NotFound)
        })
        .await
    }

    /// Applies rating operations in order as one batch of `rate`s, recorded as `actor` from
    /// `origin`: each as the rating `media.update` would set but at the operation's own time
    /// (never later than now), so the newest one wins. A refused operation (including one
    /// whose sidecar cannot be written) does not stop the others; only a failure of the whole
    /// call (the cache, or `Busy` as in [`Writer::update`]) fails it, and the client sends
    /// the whole batch again (a rating applied twice changes nothing).
    pub async fn apply_ops(
        &self,
        actor: Actor,
        origin: Origin,
        ops: Vec<RatingOp>,
    ) -> Result<Vec<OpOutcome>, WriteError> {
        let deadline = Instant::now() + self.write_wait();
        let user = organize::actor_user(actor);
        let now = Utc::now();
        let (planned, b) = self
            .run_batch(actor, origin, deadline, move |cache| {
                // Per operation: its plan, or why it is refused already.
                let planned = ops
                    .into_iter()
                    .map(|op| {
                        let plan = match parse_rating_op(&op, now) {
                            Err(reason) => Err(reason),
                            Ok(at) => match cache.get_by_id(&op.item).map_err(internal)? {
                                None => Err(RejectReason::NotFound),
                                Some(rec) if rec.sidecar_error.is_some() => {
                                    Err(RejectReason::ReadOnly)
                                }
                                Some(rec) => Ok(RatePlan {
                                    path: rec.path,
                                    user,
                                    rating: Some(RatingAt {
                                        value: op.value,
                                        at_ms: Some(at.timestamp_millis()),
                                    }),
                                    expect: None,
                                }),
                            },
                        };
                        Ok((op, plan))
                    })
                    .collect::<Result<Vec<_>, WriteError>>()?;
                let plans = planned
                    .iter()
                    .filter_map(|(_, p)| p.as_ref().ok().map(Op::new::<Rate>))
                    .collect();
                Ok((planned, plans))
            })
            .await?;
        let mut records = b.map(|b| b.ops).unwrap_or_default().into_iter();
        let cache = self.cache.clone();
        blocking(move || {
            planned
                .into_iter()
                .map(|(op, plan)| {
                    let outcome = match plan {
                        Err(reason) => Err(reason),
                        Ok(_) => match records.next().map(|r| r.outcome) {
                            Some(Outcome::Done(done)) => {
                                let changed = serde_json::from_value::<RateDone>(done)
                                    .is_ok_and(|d| d.before != d.after);
                                match cache.get_by_id(&op.item).map_err(internal)? {
                                    Some(rec) => Ok((changed, rec)),
                                    None => Err(RejectReason::NotFound),
                                }
                            }
                            Some(Outcome::Skipped(skip)) => Err(reject_of(&skip)),
                            None => {
                                return Err(WriteError::Internal(
                                    "a rating has no record in its batch".into(),
                                ));
                            }
                        },
                    };
                    Ok((op.id, outcome))
                })
                .collect()
        })
        .await
    }

    /// Plans with `plan` on the queue, then runs the plans as one batch, labelled by its
    /// first kind, and waits for it to end; no batch when there is nothing to run. A batch
    /// the queue has not started by `deadline` is `Busy` and never runs.
    async fn run_batch<T: Send + 'static>(
        &self,
        actor: Actor,
        origin: Origin,
        deadline: Instant,
        plan: impl FnOnce(&Arc<Cache>) -> Result<(T, Vec<Op>), WriteError> + Send + 'static,
    ) -> Result<(T, Option<BatchRecords>), WriteError> {
        let actor = actor_of(actor);
        let (found, id) = self
            .start_job(
                deadline,
                move |j, cache| {
                    let (found, plans) = plan(cache)?;
                    let Some(first) = plans.first() else {
                        return Ok((found, None));
                    };
                    let label = auto_label(&first.kind, plans.len());
                    Ok((found, Some(j.submit(plans, &origin, &actor, label)?)))
                },
                |_, _| {},
            )
            .await?;
        let Some(id) = id else {
            return Ok((found, None));
        };
        let journal = self.journal.clone();
        let b = blocking(move || Ok(journal.wait(id)?)).await?;
        Ok((found, Some(b)))
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

    /// Sets how long a batch may take to start, and how long [`Writer::organize`] waits for
    /// it to end (`APPLY_WAIT`; tests use a short one).
    pub(crate) fn set_write_wait(&self, wait: Duration) {
        *self.write_wait.lock().unwrap_or_else(|p| p.into_inner()) = wait;
    }

    fn write_wait(&self) -> Duration {
        *self.write_wait.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs a batch of moves and trashes in the Journal, closed by an `organize_tidy`, as
    /// `actor` from `origin`; a batch without a label gets one. The operations skipped while
    /// planning are not run, and report under their request index. A dry run checks each
    /// operation on the queue and touches nothing. The operations update the cache, so the
    /// caller need not rescan.
    ///
    /// A batch that is not over after the wait (`set_write_wait`) is answered with
    /// `running: true` and no results; it goes on, and `organize.status` reads its outcome
    /// from the Journal. One the queue has not even started by then (the queue is busy with
    /// an earlier batch) is `Busy` and never runs, as `media.update` is (§4.3). A request
    /// whose operations were all skipped while planning runs no batch and has no id. A dry
    /// run is always waited for.
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
        let deadline = Instant::now() + self.write_wait();
        let mut indices = Vec::new();
        let mut early = Vec::new();
        let mut skips = Vec::new();
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
                Err((path, why)) => {
                    early.push(organize::skipped(i, &path, why));
                    skips.push(EarlySkip {
                        index: i as u32,
                        path,
                        reason: why,
                    });
                }
            }
        }
        if indices.is_empty() {
            // Every operation was skipped while planning: nothing to run, no batch.
            return Ok(Batch {
                id: None,
                results: early,
                running: false,
            });
        }
        let label = label.or_else(|| auto_label(files::Move::KIND, indices.len()));
        plans.push(Op::new::<OrganizeTidy>(&TidyPlan {
            user: organize::actor_user(actor),
            moves,
            // For `organize.status`, which reports from the Journal alone.
            indices: indices.iter().map(|&i| i as u32).collect(),
            early: skips,
        }));
        let actor = actor_of(actor);
        let id = self
            .start_batch(
                deadline,
                move |j| j.submit(plans, &origin, &actor, label),
                |_, _| {},
            )
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
    /// someone else's) is refused at once, before the queue; one the queue does not start
    /// within the wait is `Busy`, as in [`Writer::organize`].
    pub(crate) async fn undo(
        &self,
        of: BatchId,
        actor: Actor,
        origin: Origin,
        ext: Arc<dyn AppExtension>,
        dry_run: bool,
    ) -> Result<Batch, WriteError> {
        let deadline = Instant::now() + self.write_wait();
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
                deadline,
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
    ///
    /// A job the queue has not taken by `deadline` is `Busy`, and never runs: the queue and
    /// the caller each try to claim it, and only one can.
    async fn start_batch(
        &self,
        deadline: Instant,
        start: impl FnOnce(&ServerJournal) -> Result<BatchId, JournalError> + Send + 'static,
        ended: impl FnOnce(&Arc<Library>, &BatchRecords) + Send + 'static,
    ) -> Result<BatchId, WriteError> {
        let ((), id) = self
            .start_job(deadline, move |j, _| Ok(((), Some(start(j)?))), ended)
            .await?;
        id.ok_or_else(|| WriteError::Internal("the batch did not start".into()))
    }

    /// [`Writer::start_batch`] for a `start` that plans on the queue, with the cache as it is
    /// then, and may find nothing to run: it gives what it found, and the batch's id if any.
    async fn start_job<T: Send + 'static>(
        &self,
        deadline: Instant,
        start: impl FnOnce(&ServerJournal, &Arc<Cache>) -> Result<(T, Option<BatchId>), WriteError>
        + Send
        + 'static,
        ended: impl FnOnce(&Arc<Library>, &BatchRecords) + Send + 'static,
    ) -> Result<(T, Option<BatchId>), WriteError> {
        let (started, rx) = oneshot::channel();
        let journal = self.journal.clone();
        let claim = Arc::new(Claim::default());
        let queue_claim = claim.clone();
        let job = Box::new(move |lib: &Arc<Library>, cache: &Arc<Cache>| {
            if !queue_claim.take(Claim::RUN) {
                return;
            }
            let (found, id) = match start(&journal, cache) {
                Ok(v) => v,
                Err(e) => {
                    let _ = started.send(Err(e));
                    return;
                }
            };
            let _ = started.send(Ok((found, id)));
            let Some(id) = id else { return };
            match journal.wait(id) {
                Ok(b) => ended(lib, &b),
                Err(e) => tracing::error!(error = %e, batch = %id, "a batch failed"),
            }
        });
        let busy = || WriteError::Busy("the batch did not start in time".into());
        match tokio::time::timeout_at(deadline, self.tx.send(Job::Exec(job))).await {
            Err(_) => return Err(busy()),
            Ok(sent) => sent.map_err(|_| stopped())?,
        }
        let mut rx = rx;
        let started = match tokio::time::timeout_at(deadline, &mut rx).await {
            Ok(started) => started,
            // Not taken yet: refuse it, unless the queue has just taken it.
            Err(_) if claim.take(Claim::DROP) => return Err(busy()),
            Err(_) => rx.await,
        };
        // A dropped sender means the job panicked.
        started.map_err(|_| WriteError::Internal("write job failed".into()))?
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

    /// Moves orphaned sidecars to their items (see [`crate::hashing::relink_orphans`]): one
    /// batch from the scan, which this job waits for.
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
    /// so other writes go on meanwhile; only the renames wait their turn, as one batch of the
    /// Journal by `user` from `origin`. Does not rescan; the caller asks for that.
    ///
    /// The commit runs in a task of its own: once started it finishes, and leaves its
    /// tombstones, even when the caller stops waiting (a client that timed out). A repeated
    /// commit meanwhile is `Busy` (the sessions are sealed), and afterwards gets the stored
    /// answer.
    pub async fn commit_upload(
        &self,
        store: Arc<UploadStore>,
        user: UserId,
        origin: Origin,
        params: UploadCommitParams,
    ) -> Result<UploadCommitResult, UploadError> {
        let writer = self.clone();
        let task =
            tokio::spawn(async move { writer.run_commit(store, user, origin, params).await });
        task.await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "upload commit task failed");
            Err(UploadError::job_failed())
        })
    }

    async fn run_commit(
        &self,
        store: Arc<UploadStore>,
        user: UserId,
        origin: Origin,
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
                prepared: Box::new(prepared),
                origin,
                reply,
            })
            .await
            .map_err(|_| upload_stopped())?;
        rx.await.map_err(|_| upload_stopped())?
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

/// The time of `op`, held back to `now`; or why the operation is invalid.
fn parse_rating_op(op: &RatingOp, now: DateTime<Utc>) -> Result<DateTime<Utc>, RejectReason> {
    if !(-1..=5).contains(&op.value) {
        return Err(RejectReason::Invalid);
    }
    let at = DateTime::parse_from_rfc3339(&op.at).map_err(|_| RejectReason::Invalid)?;
    Ok(at.with_timezone(&Utc).min(now))
}

/// Why a `rate` the Journal skipped is refused: a sidecar that cannot be written makes the
/// item read-only.
fn reject_of(skip: &Skip) -> RejectReason {
    match skip {
        Skip::NotFound => RejectReason::NotFound,
        Skip::InvalidPlan(_) => RejectReason::Invalid,
        _ => RejectReason::ReadOnly,
    }
}

/// Runs blocking work (the cache, the Journal) off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, WriteError> + Send + 'static,
) -> Result<T, WriteError> {
    run_blocking(f).await
}
