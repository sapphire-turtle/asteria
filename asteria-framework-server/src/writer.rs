//! The single per-library write queue. Every write to original files (sidecar edits and
//! orphan relinks) goes through one task that runs jobs one at a time on the blocking
//! pool, then refreshes the cache.
//!
//! Shutdown: [`spawn_writer`] returns the [`Writer`] and the task's `JoinHandle`. Once
//! every `Writer` clone is dropped the task finishes the queued jobs and ends, so
//! `drop(writers); handle.await` drains the queue.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use asteria_framework::journal::Actor;
use asteria_framework::{Error as FwError, Library, UserId};
use asteria_framework_api::{ItemId, MediaUpdateParams};
use chrono::Utc;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cache::{Cache, ItemRecord, SidecarSnapshot, StoredHash};
use crate::error::ServerError;
use crate::hashing::{RelinkReport, relink_orphans};

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
}

#[derive(Clone)]
pub struct Writer {
    tx: mpsc::Sender<Job>,
}

/// Starts the writer task. See the module docs for shutdown.
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
            }
        }
    });
    (Writer { tx }, handle)
}

async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, WriteError> + Send + 'static,
) -> Result<T, WriteError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "write job panicked");
            Err(WriteError::Internal("write job failed".into()))
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

    pub async fn relink_orphans(&self) -> Result<RelinkReport, WriteError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Relink { reply })
            .await
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }
}

fn map_fw(e: FwError) -> WriteError {
    match e {
        FwError::MediaNotFound(_) => WriteError::NotFound,
        FwError::InvalidPath(p) => WriteError::InvalidPath(p.display().to_string()),
        e @ FwError::MalformedSidecar { .. } => WriteError::ReadOnly(e.to_string()),
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

/// The cached hash, usable only if it was computed for the record's current size/mtime.
fn usable_hash(
    rec: &ItemRecord,
) -> Option<(
    asteria_framework::hash::ContentHash,
    asteria_framework::hash::HashScope,
)> {
    let h: &StoredHash = rec.hash.as_ref()?;
    if h.size != rec.size || h.mtime_ns != rec.mtime_ns {
        return None;
    }
    Some((h.hash.parse().ok()?, h.scope.parse().ok()?))
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
    let actor = Actor::User(user);

    let has_fields = params.subjects.is_some()
        || params.hierarchical_subjects.is_some()
        || params.description.is_some()
        || params.label.is_some();
    // Fields first, so the pre-set cached hash is written along and the later
    // set_rating finds a hash already present.
    if has_fields {
        let hash = usable_hash(&rec);
        library
            .update_sidecar(rel, actor, |sc| {
                if sc.content_hash.is_none() {
                    sc.content_hash = hash;
                }
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
            })
            .map_err(map_fw)?;
    }
    if let Some(value) = params.rating {
        library
            .set_rating(rel, user, value, Utc::now(), actor)
            .map_err(map_fw)?;
    }

    let snapshot = match library.read_sidecar(rel).map_err(map_fw)? {
        Some(sc) => SidecarSnapshot::from(&sc),
        None => SidecarSnapshot::default(),
    };
    cache
        .set_sidecar(&rec.path, snapshot, None)
        .map_err(internal)?;
    cache
        .get_by_id(&id)
        .map_err(internal)?
        .ok_or(WriteError::NotFound)
}
