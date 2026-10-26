//! The upload queue kept on disk, so an upload resumes after the app restarts (spec §5.3).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use asteria_framework_api::HashRef;
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use super::plan::DropPlan;
use crate::fsutil::{bad_path, write_atomic};
use crate::media_cache::safe_component;

const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupId(pub GrainId);

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for GroupId {
    type Err = <GrainId as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(GroupId)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedFile {
    pub local: PathBuf,
    /// `/`-separated, relative to `Inbox/`.
    pub inbox_path: String,
    /// 0 until the engine has hashed the file.
    pub size: u64,
    pub mtime_ns: i128,
    pub file_hash: Option<String>,
    pub payload: Option<HashRef>,
    /// The server's upload session, once one is open.
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GroupState {
    Hashing,
    Checking,
    AwaitingDecision {
        existing: Vec<String>,
    },
    Uploading {
        allow_duplicate: bool,
    },
    Committing {
        allow_duplicate: bool,
    },
    Done {
        placed: Vec<String>,
        warnings: Vec<String>,
    },
    Discarded,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedGroup {
    pub id: GroupId,
    pub files: Vec<QueuedFile>,
    pub state: GroupState,
    /// Bytes the engine has confirmed sent.
    #[serde(default)]
    pub sent: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UploadQueue {
    pub groups: Vec<QueuedGroup>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueSummary {
    pub total: usize,
    pub done: usize,
    pub awaiting: usize,
    pub failed: usize,
    /// The group being sent: its id, bytes sent and total bytes.
    pub active: Option<(GroupId, u64, u64)>,
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    groups: &'a [QueuedGroup],
}

#[derive(Deserialize)]
struct FileIn {
    version: u32,
    groups: Vec<QueuedGroup>,
}

/// [`UploadQueue::save`] for a queue that is only borrowed.
pub(crate) fn save_groups(groups: &[QueuedGroup], path: &Path) -> io::Result<()> {
    let bytes = serde_json::to_vec(&FileOut {
        version: VERSION,
        groups,
    })
    .map_err(io::Error::other)?;
    write_atomic(path, &bytes, false)
}

/// `<data_root>/uploads/<library_id>.json`, or `None` if the id is not a safe file name.
pub fn queue_path(data_root: &Path, library_id: &str) -> Option<PathBuf> {
    safe_component(library_id).then(|| data_root.join("uploads").join(format!("{library_id}.json")))
}

impl UploadQueue {
    /// A missing file is an empty queue. A file that is malformed, or written by another version,
    /// is moved to `<name>.bad-<unix time>` and the queue starts empty. Any other read error is
    /// returned: starting empty there would let the next save overwrite the real queue.
    pub fn load(path: &Path) -> io::Result<UploadQueue> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(UploadQueue::default()),
            Err(e) => return Err(e),
        };
        let reason = match serde_json::from_slice::<FileIn>(&bytes) {
            Ok(f) if f.version == VERSION => return Ok(UploadQueue { groups: f.groups }),
            Ok(f) => format!("written by queue version {} (this is {VERSION})", f.version),
            Err(e) => format!("not a valid upload queue: {e}"),
        };
        let aside = bad_path(path);
        tracing::warn!(
            path = %path.display(),
            moved_to = %aside.display(),
            %reason,
            "upload queue cannot be used; set it aside and starting empty"
        );
        fs::rename(path, &aside)?;
        Ok(UploadQueue::default())
    }

    /// Temp file in the same directory, then rename.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        save_groups(&self.groups, path)
    }

    /// Adds each planned group, ready to be hashed.
    pub fn push(&mut self, plan: DropPlan) {
        for g in plan.groups {
            self.groups.push(QueuedGroup {
                id: GroupId(GrainId::random()),
                files: g
                    .files
                    .into_iter()
                    .map(|f| QueuedFile {
                        local: f.local,
                        inbox_path: f.inbox_path,
                        size: 0,
                        mtime_ns: 0,
                        file_hash: None,
                        payload: None,
                        session: None,
                    })
                    .collect(),
                state: GroupState::Hashing,
                sent: 0,
            });
        }
    }

    /// Drops the `Done` and `Discarded` groups.
    pub fn clear_finished(&mut self) {
        self.groups
            .retain(|g| !matches!(g.state, GroupState::Done { .. } | GroupState::Discarded));
    }

    /// Answers a group waiting on a duplicate: keep it (upload anyway) or discard it. Returns
    /// whether anything changed.
    pub fn decide(&mut self, group: GroupId, keep: bool) -> bool {
        match self.groups.iter_mut().find(|g| g.id == group) {
            Some(g) => decide_one(g, keep),
            None => false,
        }
    }

    /// [`decide`](Self::decide) for every group that is waiting.
    pub fn decide_all(&mut self, keep: bool) -> bool {
        let mut changed = false;
        for g in &mut self.groups {
            changed |= decide_one(g, keep);
        }
        changed
    }

    /// Puts a `Failed` group back to `Hashing`.
    pub fn retry(&mut self, group: GroupId) -> bool {
        match self.groups.iter_mut().find(|g| g.id == group) {
            Some(g) if matches!(g.state, GroupState::Failed { .. }) => {
                g.state = GroupState::Hashing;
                g.sent = 0;
                true
            }
            _ => false,
        }
    }

    /// Discards a group that is not yet `Done` or `Discarded`. Aborting its sessions is the engine's job.
    pub fn cancel(&mut self, group: GroupId) -> bool {
        match self.groups.iter_mut().find(|g| g.id == group) {
            Some(g) if !matches!(g.state, GroupState::Discarded | GroupState::Done { .. }) => {
                g.state = GroupState::Discarded;
                true
            }
            _ => false,
        }
    }

    pub fn summary(&self) -> QueueSummary {
        let count = |f: fn(&GroupState) -> bool| self.groups.iter().filter(|g| f(&g.state)).count();
        QueueSummary {
            total: self.groups.len(),
            done: count(|s| matches!(s, GroupState::Done { .. })),
            awaiting: count(|s| matches!(s, GroupState::AwaitingDecision { .. })),
            failed: count(|s| matches!(s, GroupState::Failed { .. })),
            active: self
                .groups
                .iter()
                .find(|g| {
                    matches!(
                        g.state,
                        GroupState::Uploading { .. } | GroupState::Committing { .. }
                    )
                })
                .map(|g| (g.id, g.sent, g.files.iter().map(|f| f.size).sum())),
        }
    }
}

fn decide_one(g: &mut QueuedGroup, keep: bool) -> bool {
    if !matches!(g.state, GroupState::AwaitingDecision { .. }) {
        return false;
    }
    g.state = if keep {
        GroupState::Uploading {
            allow_duplicate: true,
        }
    } else {
        GroupState::Discarded
    };
    true
}
