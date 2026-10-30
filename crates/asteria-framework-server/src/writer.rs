//! The single per-library write queue. Every write to original files (sidecar edits,
//! orphan relinks and placing upload commits) goes through one task that runs jobs one at a
//! time on the blocking pool, then refreshes the cache. An upload commit hashes and stages
//! its files before it joins the queue (see [`crate::upload_commit`]).
//!
//! Shutdown: [`spawn_writer`] returns the [`Writer`] and the task's `JoinHandle`. Once
//! every `Writer` clone is dropped the task finishes the queued jobs and ends, so
//! `drop(writers); handle.await` drains the queue. When clones may outlive the shutdown
//! (a connection task still holding the app state), [`Writer::close`] refuses new jobs and
//! lets the task end after the queued ones.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use asteria_framework::hash::{ContentHash, HashScope};
use asteria_framework::journal::{Actor, BatchId};
use asteria_framework::{Error as FwError, Library, UserId};
use asteria_framework_api::{
    ItemId, MediaUpdateParams, OpStatus, SkipReason, UploadCommitParams, UploadCommitResult,
};
use chrono::Utc;
use grain_id::GrainId;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cache::{Cache, ItemRecord, SidecarSnapshot, StoredHash};
use crate::error::ServerError;
use crate::ext::{AppExtension, ExtContext, Rewrite};
use crate::hashing::{RelinkReport, relink_orphans};
use crate::organize::{self, Batch, OpCtx, Planned, Target};
use crate::scan::mtime_ns;
use crate::upload::{UploadError, UploadStore};
use crate::upload_commit::{Begun, Prepared, place, prepare_sealed, seal_group};

const QUEUE: usize = 256;
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
}

/// A piece of work for the queue: it gets the library and cache and reports on its own.
type ExecFn = Box<dyn FnOnce(&Arc<Library>, &Arc<Cache>) + Send>;

enum Job {
    Update {
        user: UserId,
        params: MediaUpdateParams,
        reply: oneshot::Sender<Result<ItemRecord, WriteError>>,
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
    /// A batch that undoes `of`; `restores` put playlists back after the moves. `claim`
    /// keeps `of` marked as running until the batch has ended, so no second undo of it can
    /// start in between.
    Undo {
        of: BatchId,
        restores: Vec<Rewrite>,
        claim: RunningGuard,
    },
}

#[derive(Clone)]
pub struct Writer {
    tx: mpsc::Sender<Job>,
    library: Arc<Library>,
    /// The batches being run right now.
    running: Arc<Mutex<HashSet<BatchId>>>,
    /// Turns for the first phase of upload commits (`COMMIT_PREPARE_JOBS`).
    prepare_permits: Arc<Semaphore>,
}

/// Starts the writer task. Must be called inside a Tokio runtime (it uses `tokio::spawn`).
/// See the module docs for shutdown.
pub fn spawn_writer(library: Arc<Library>, cache: Arc<Cache>) -> (Writer, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Job>(QUEUE);
    let writer = Writer {
        tx,
        library: library.clone(),
        running: Arc::default(),
        prepare_permits: Arc::new(Semaphore::new(COMMIT_PREPARE_JOBS)),
    };
    let handle = tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let (lib, cache) = (library.clone(), cache.clone());
            match job {
                Job::Update {
                    user,
                    params,
                    reply,
                } => {
                    let r = run_blocking(move || do_update(&lib, &cache, user, params)).await;
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
    pub async fn update(
        &self,
        user: UserId,
        params: MediaUpdateParams,
    ) -> Result<ItemRecord, WriteError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Update {
                user,
                params,
                reply,
            })
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

    /// The batches that are running now (between their first and last journal line).
    pub fn running(&self) -> Arc<Mutex<HashSet<BatchId>>> {
        self.running.clone()
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

    /// Marks `id` as running, or `None` if it already is. `organize.undo` takes this on the
    /// batch it undoes before reading the journal, so the check for an earlier undo and the
    /// start of this one cannot interleave with another undo of the same batch.
    pub(crate) fn claim_running(&self, id: BatchId) -> Option<RunningGuard> {
        RunningGuard::try_new(&self.running, id)
    }

    /// Runs a batch of moves and trashes: a header line, then each operation as a job of its
    /// own (so other writes interleave), then a last job that tidies up and journals the
    /// end. A dry run is one job that touches nothing. The batch runs in a task of its own
    /// and finishes even when the caller stops waiting. The cache is updated by the
    /// operations, so the caller need not rescan.
    ///
    /// An error means nothing was done: the header could not be journaled (for example the
    /// server has no state directory), so no file was touched.
    pub(crate) async fn organize(
        &self,
        actor: Actor,
        label: Option<String>,
        ops: Vec<Planned>,
        ext: Arc<dyn AppExtension>,
        kind: BatchKind,
    ) -> Result<Batch, WriteError> {
        let writer = self.clone();
        let task =
            tokio::spawn(async move { writer.run_batch(actor, label, ops, ext, kind).await });
        task.await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "organize task failed");
            Err(WriteError::Internal("write job failed".into()))
        })
    }

    async fn run_batch(
        &self,
        actor: Actor,
        label: Option<String>,
        ops: Vec<Planned>,
        ext: Arc<dyn AppExtension>,
        kind: BatchKind,
    ) -> Result<Batch, WriteError> {
        let (undo_of, restores, _claim) = match kind {
            BatchKind::DryRun => {
                let results = self
                    .exec(move |lib, cache| {
                        let ctx = OpCtx {
                            library: lib,
                            cache,
                            ext: ext.as_ref(),
                            actor,
                            batch: None,
                        };
                        organize::dry_run(&ctx, &ops)
                    })
                    .await?;
                return Ok(Batch { id: None, results });
            }
            BatchKind::Apply => (None, None, None),
            BatchKind::Undo {
                of,
                restores,
                claim,
            } => (Some(of), Some(restores), Some(claim)),
        };

        let id = BatchId(GrainId::random());
        let _running = RunningGuard::new(&self.running, id);
        let count = ops.len();
        let header = {
            let ext = ext.clone();
            self.exec(move |lib, cache| {
                let ctx = OpCtx {
                    library: lib,
                    cache,
                    ext: ext.as_ref(),
                    actor,
                    batch: Some(id),
                };
                organize::write_header(&ctx, label.as_deref(), count, undo_of)
            })
            .await?
        };
        if let Err(e) = header {
            tracing::error!(error = %e, "cannot journal the start of a batch; nothing was changed");
            return Err(WriteError::Internal(e.to_string()));
        }

        let mut results = Vec::with_capacity(count);
        // (from, to) of the completed operations; `to` is `None` for a trash.
        let mut completed: Vec<(String, Option<String>)> = Vec::new();
        for (i, planned) in ops.into_iter().enumerate() {
            let op = match planned {
                Ok(op) => op,
                Err((path, why)) => {
                    results.push(organize::skipped(i, &path, why));
                    continue;
                }
            };
            let (job_op, job_ext) = (op.clone(), ext.clone());
            let outcome = self
                .exec(move |lib, cache| {
                    let ctx = OpCtx {
                        library: lib,
                        cache,
                        ext: job_ext.as_ref(),
                        actor,
                        batch: Some(id),
                    };
                    organize::apply(&ctx, &job_op)
                })
                .await
                .unwrap_or_else(|e| {
                    tracing::error!(error = %e, from = %op.from, "organize job did not run");
                    Err(SkipReason::Io)
                });
            match outcome {
                Ok(d) => {
                    let to = match op.to {
                        Target::Path(_) => Some(d.to.clone()),
                        Target::Trash => None,
                    };
                    completed.push((op.from.clone(), to));
                    results.push(organize::applied(i, &op, d));
                }
                Err(why) => results.push(organize::skipped(i, &op.from, why)),
            }
        }

        let done = results
            .iter()
            .filter(|r| r.status == OpStatus::Done)
            .count();
        let skipped = results.len() - done;
        let finished = self
            .exec(move |lib, cache| {
                let ctx = OpCtx {
                    library: lib,
                    cache,
                    ext: ext.as_ref(),
                    actor,
                    batch: Some(id),
                };
                let rewrites = match restores {
                    Some(restores) => restores,
                    None => {
                        let moves: Vec<(String, String)> = completed
                            .iter()
                            .filter_map(|(from, to)| Some((from.clone(), to.clone()?)))
                            .collect();
                        let ectx = ExtContext {
                            library: lib.clone(),
                            cache: cache.clone(),
                            user: organize::actor_user(actor),
                            owner: lib.owner(),
                        };
                        organize::rewrites_after(ext.as_ref(), &ectx, &moves)
                    }
                };
                let sources: Vec<String> = completed.into_iter().map(|(from, _)| from).collect();
                organize::finish(&ctx, &sources, &rewrites, done, skipped);
            })
            .await;
        if let Err(e) = finished {
            tracing::error!(error = %e, "the last job of a batch did not run");
        }
        Ok(Batch {
            id: Some(id),
            results,
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

/// Takes a batch out of `running` however its task ends.
pub(crate) struct RunningGuard {
    set: Arc<Mutex<HashSet<BatchId>>>,
    id: BatchId,
}

impl RunningGuard {
    fn try_new(set: &Arc<Mutex<HashSet<BatchId>>>, id: BatchId) -> Option<RunningGuard> {
        let fresh = set.lock().unwrap_or_else(|p| p.into_inner()).insert(id);
        fresh.then(|| RunningGuard {
            set: set.clone(),
            id,
        })
    }

    fn new(set: &Arc<Mutex<HashSet<BatchId>>>, id: BatchId) -> RunningGuard {
        set.lock().unwrap_or_else(|p| p.into_inner()).insert(id);
        RunningGuard {
            set: set.clone(),
            id,
        }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
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
    user: UserId,
    params: MediaUpdateParams,
) -> Result<ItemRecord, WriteError> {
    validate(&params)?;
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
    let written = library.update_sidecar(rel, Actor::User(user), |sc| {
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
