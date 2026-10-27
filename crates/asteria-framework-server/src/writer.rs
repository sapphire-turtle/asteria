//! The single per-library write queue. Every write to original files (sidecar edits,
//! orphan relinks and placing upload commits) goes through one task that runs jobs one at a
//! time on the blocking pool, then refreshes the cache. An upload commit hashes its files
//! before it joins the queue (see [`crate::upload_commit`]).
//!
//! Shutdown: [`spawn_writer`] returns the [`Writer`] and the task's `JoinHandle`. Once
//! every `Writer` clone is dropped the task finishes the queued jobs and ends, so
//! `drop(writers); handle.await` drains the queue. When clones may outlive the shutdown
//! (a connection task still holding the app state), [`Writer::close`] refuses new jobs and
//! lets the task end after the queued ones.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use asteria_framework::hash::{ContentHash, HashScope};
use asteria_framework::journal::Actor;
use asteria_framework::{Error as FwError, Library, UserId};
use asteria_framework_api::{ItemId, MediaUpdateParams, UploadCommitParams, UploadCommitResult};
use chrono::Utc;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cache::{Cache, ItemRecord, SidecarSnapshot, StoredHash};
use crate::error::ServerError;
use crate::hashing::{RelinkReport, relink_orphans};
use crate::scan::mtime_ns;
use crate::upload::{UploadError, UploadStore};
use crate::upload_commit::{Prepare, Prepared, place, prepare};

const QUEUE: usize = 256;

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
    /// Refuse new jobs; finish the queued ones and end.
    Close,
}

#[derive(Clone)]
pub struct Writer {
    tx: mpsc::Sender<Job>,
}

/// Starts the writer task. Must be called inside a Tokio runtime (it uses `tokio::spawn`).
/// See the module docs for shutdown.
pub fn spawn_writer(library: Arc<Library>, cache: Arc<Cache>) -> (Writer, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Job>(QUEUE);
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
                Job::Close => rx.close(),
            }
        }
    });
    (Writer { tx }, handle)
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

    pub async fn relink_orphans(&self) -> Result<RelinkReport, WriteError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Relink { reply })
            .await
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// Places a group of finished upload sessions under `Inbox/` (see
    /// [`crate::upload_commit`]). The sessions are checked and hashed first, on the blocking
    /// pool but outside the queue, so other writes go on meanwhile; only the placement waits
    /// its turn. Does not rescan; the caller asks for that.
    pub async fn commit_upload(
        &self,
        store: Arc<UploadStore>,
        user: UserId,
        params: UploadCommitParams,
    ) -> Result<UploadCommitResult, UploadError> {
        let upload_stopped = || UploadError::Io("writer stopped".into());
        let sealing = store.clone();
        let prepared = run_blocking(move || prepare(&sealing, user, params)).await?;
        let prepared = match prepared {
            Prepare::Answered(result) => return Ok(result),
            Prepare::Ready(p) => p,
        };
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
