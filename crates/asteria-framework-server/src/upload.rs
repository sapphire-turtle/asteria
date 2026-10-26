//! Resumable upload sessions under `<root>/.asteria/uploads/<session>/`.
//!
//! A session directory holds `session.json` (owner, size, claimed hash), `data`
//! (pre-sized to the file's length; chunks are written in place) and `received`
//! (one byte per chunk, 1 once that chunk is in `data`).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use asteria_framework::hash::ContentHash;
use asteria_framework::ids::UserId;
use asteria_framework_api::{UPLOAD_CHUNK_SIZE, UPLOAD_MAX_FILE, UploadBegin, UploadStatus};
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use crate::error::ServerError;

/// A session that has received no chunk for this long is removed.
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("no such upload session")]
    NotFound,
    #[error("invalid upload: {0}")]
    Invalid(String),
    #[error("upload quota exceeded: {0}")]
    Quota(String),
    #[error("upload is incomplete")]
    Incomplete,
    #[error("file hash does not match")]
    HashMismatch,
    #[error("upload storage error: {0}")]
    Io(String),
}

impl From<std::io::Error> for UploadError {
    fn from(e: std::io::Error) -> Self {
        UploadError::Io(e.to_string())
    }
}

/// Per-user ceilings on unfinished sessions.
#[derive(Debug, Clone, Copy)]
pub struct UploadLimits {
    /// Total `size` of the user's unfinished sessions.
    pub max_pending_bytes: u64,
    pub max_sessions: usize,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            max_pending_bytes: 64 * 1024 * 1024 * 1024,
            max_sessions: 1000,
        }
    }
}

/// A fully received file, still in its session.
#[derive(Debug, Clone)]
pub struct CompletedUpload {
    pub data_path: PathBuf,
    pub size: u64,
    /// The client's claim; the committer hashes `data_path` itself.
    pub file_hash: ContentHash,
}

#[derive(Serialize, Deserialize)]
struct SessionFile {
    owner: String,
    size: u64,
    file_hash: String,
    /// Seconds since the Unix epoch.
    created: u64,
}

pub struct UploadStore {
    dir: PathBuf,
    limits: UploadLimits,
    /// Serializes writes to one session; also held across `begin`'s quota check.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    begin_lock: Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn chunk_count(size: u64) -> u64 {
    size.div_ceil(UPLOAD_CHUNK_SIZE)
}

/// A session id is the canonical text of a `GrainId`; anything else is not ours.
fn is_session_id(name: &str) -> bool {
    name.parse::<GrainId>()
        .is_ok_and(|id| id.to_string() == name)
}

impl UploadStore {
    pub fn open(root: &Path) -> Result<Self, ServerError> {
        Self::open_with_limits(root, UploadLimits::default())
    }

    pub fn open_with_limits(root: &Path, limits: UploadLimits) -> Result<Self, ServerError> {
        let dir = root.join(".asteria").join("uploads");
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            limits,
            locks: Mutex::new(HashMap::new()),
            begin_lock: Mutex::new(()),
        })
    }

    pub fn begin(
        &self,
        owner: UserId,
        size: u64,
        file_hash: ContentHash,
    ) -> Result<UploadBegin, UploadError> {
        if size == 0 || size > UPLOAD_MAX_FILE {
            return Err(UploadError::Invalid(format!(
                "size must be between 1 and {UPLOAD_MAX_FILE} bytes"
            )));
        }
        let _guard = lock(&self.begin_lock);
        let owner_s = owner.to_string();
        let (mut pending, mut count) = (size, 1usize);
        for name in self.session_names()? {
            if let Ok(s) = self.read_session(&name)
                && s.owner == owner_s
            {
                pending = pending.saturating_add(s.size);
                count += 1;
            }
        }
        if count > self.limits.max_sessions {
            return Err(UploadError::Quota(format!(
                "at most {} unfinished uploads",
                self.limits.max_sessions
            )));
        }
        if pending > self.limits.max_pending_bytes {
            return Err(UploadError::Quota(format!(
                "at most {} bytes of unfinished uploads",
                self.limits.max_pending_bytes
            )));
        }

        let id = GrainId::random().to_string();
        let dir = self.dir.join(&id);
        let built = (|| -> Result<(), UploadError> {
            fs::create_dir(&dir)?;
            File::create(dir.join("data"))?.set_len(size)?;
            let mut received = File::create(dir.join("received"))?;
            received.write_all(&vec![0u8; chunk_count(size) as usize])?;
            received.sync_all()?;
            let session = SessionFile {
                owner: owner_s,
                size,
                file_hash: file_hash.to_string(),
                created: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
            };
            let tmp = dir.join("session.json.tmp");
            fs::write(
                &tmp,
                serde_json::to_vec(&session).map_err(|e| UploadError::Io(e.to_string()))?,
            )?;
            fs::rename(&tmp, dir.join("session.json"))?;
            Ok(())
        })();
        if let Err(e) = built {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok(UploadBegin {
            session: id,
            chunk_size: UPLOAD_CHUNK_SIZE,
        })
    }

    pub fn put_chunk(
        &self,
        owner: UserId,
        session: &str,
        index: u64,
        body: &[u8],
    ) -> Result<(), UploadError> {
        let s = self.owned(owner, session)?;
        let chunks = chunk_count(s.size);
        if index >= chunks {
            return Err(UploadError::Invalid(format!(
                "chunk {index} out of range (0..{chunks})"
            )));
        }
        let expected = if index + 1 == chunks {
            s.size - index * UPLOAD_CHUNK_SIZE
        } else {
            UPLOAD_CHUNK_SIZE
        };
        if body.len() as u64 != expected {
            return Err(UploadError::Invalid(format!(
                "chunk {index} must be {expected} bytes, got {}",
                body.len()
            )));
        }

        self.locked(session, |dir| Self::write_chunk(dir, index, body))
    }

    fn write_chunk(dir: &Path, index: u64, body: &[u8]) -> Result<(), UploadError> {
        let mut data = OpenOptions::new().write(true).open(dir.join("data"))?;
        data.seek(SeekFrom::Start(index * UPLOAD_CHUNK_SIZE))?;
        data.write_all(body)?;
        data.sync_data()?;
        // The marker comes after the data, so a crash never claims a chunk we lack.
        // Writing it also refreshes the mtime that `purge_expired` goes by.
        let mut received = OpenOptions::new().write(true).open(dir.join("received"))?;
        received.seek(SeekFrom::Start(index))?;
        received.write_all(&[1])?;
        received.sync_data()?;
        Ok(())
    }

    pub fn status(&self, owner: UserId, session: &str) -> Result<UploadStatus, UploadError> {
        let s = self.owned(owner, session)?;
        let missing = self
            .read_received(session, s.size)?
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == 0)
            .map(|(i, _)| i as u64)
            .collect();
        Ok(UploadStatus {
            size: s.size,
            missing,
        })
    }

    pub fn abort(&self, owner: UserId, session: &str) -> Result<(), UploadError> {
        self.owned(owner, session)?;
        self.remove(session);
        Ok(())
    }

    pub fn completed(&self, owner: UserId, session: &str) -> Result<CompletedUpload, UploadError> {
        let s = self.owned(owner, session)?;
        if self.read_received(session, s.size)?.contains(&0) {
            return Err(UploadError::Incomplete);
        }
        let file_hash = s
            .file_hash
            .parse()
            .map_err(|_| UploadError::Io("session.json holds a bad hash".into()))?;
        Ok(CompletedUpload {
            data_path: self.dir.join(session).join("data"),
            size: s.size,
            file_hash,
        })
    }

    /// Drops a session's files, for after its file was committed.
    pub fn remove(&self, session: &str) {
        if !is_session_id(session) {
            return;
        }
        let session_lock = self.session_lock(session);
        let _guard = lock(&session_lock);
        let _ = fs::remove_dir_all(self.dir.join(session));
        drop(_guard);
        lock(&self.locks).remove(session);
    }

    /// Removes sessions with no chunk for 7 days, plus anything that is not a
    /// readable session. Returns how many were removed.
    pub fn purge_expired(&self, now: SystemTime) -> usize {
        // `begin` creates a directory before its session.json exists; wait it out.
        let _begin = lock(&self.begin_lock);
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let stale = if !is_session_id(&name) || self.read_session(&name).is_err() {
                true
            } else {
                // `received` is rewritten on every chunk; its mtime is the last activity.
                fs::metadata(path.join("received"))
                    .and_then(|m| m.modified())
                    .map(|t| now.duration_since(t).is_ok_and(|age| age > SESSION_TTL))
                    .unwrap_or(true)
            };
            if !stale {
                continue;
            }
            let ok = if path.is_dir() {
                fs::remove_dir_all(&path).is_ok()
            } else {
                fs::remove_file(&path).is_ok()
            };
            if ok {
                lock(&self.locks).remove(&name);
                removed += 1;
            }
        }
        removed
    }

    fn session_lock(&self, session: &str) -> Arc<Mutex<()>> {
        lock(&self.locks)
            .entry(session.to_owned())
            .or_default()
            .clone()
    }

    fn session_names(&self) -> Result<Vec<String>, UploadError> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if is_session_id(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }

    fn read_session(&self, session: &str) -> Result<SessionFile, UploadError> {
        let mut text = String::new();
        File::open(self.dir.join(session).join("session.json"))?.read_to_string(&mut text)?;
        serde_json::from_str(&text).map_err(|e| UploadError::Io(e.to_string()))
    }

    fn read_received(&self, session: &str, size: u64) -> Result<Vec<u8>, UploadError> {
        let received = self.locked(session, |dir| Ok(fs::read(dir.join("received"))?))?;
        if received.len() as u64 != chunk_count(size) {
            return Err(UploadError::Io("received map has the wrong length".into()));
        }
        Ok(received)
    }

    /// Runs `f` on the session directory under the session's lock. A session that is
    /// gone by then (aborted or purged meanwhile) is `NotFound`, and leaves no lock behind.
    fn locked<T>(
        &self,
        session: &str,
        f: impl FnOnce(&Path) -> Result<T, UploadError>,
    ) -> Result<T, UploadError> {
        let session_lock = self.session_lock(session);
        let _guard = lock(&session_lock);
        let dir = self.dir.join(session);
        if !dir.join("session.json").is_file() {
            lock(&self.locks).remove(session);
            return Err(UploadError::NotFound);
        }
        f(&dir)
    }

    /// The session, if it exists and belongs to `owner`; otherwise `NotFound`.
    fn owned(&self, owner: UserId, session: &str) -> Result<SessionFile, UploadError> {
        if !is_session_id(session) {
            return Err(UploadError::NotFound);
        }
        match self.read_session(session) {
            Ok(s) if s.owner == owner.to_string() => Ok(s),
            Ok(_) => Err(UploadError::NotFound),
            Err(UploadError::Io(_)) if !self.dir.join(session).join("session.json").exists() => {
                Err(UploadError::NotFound)
            }
            Err(e) => Err(e),
        }
    }
}
