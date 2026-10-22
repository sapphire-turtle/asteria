//! Append-only JSONL journal of file operations, one `YYYY-MM.jsonl` file per month (UTC).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::UserId;
use crate::library::Library;
use crate::paths::META_DIR;

const JOURNAL_DIR: &str = "journal";
const EXT: &str = "jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpId(pub GrainId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(pub GrainId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ActorRepr", into = "ActorRepr")]
pub enum Actor {
    User(UserId),
    Agent { on_behalf_of: UserId },
}

/// Wire form of [`Actor`]: `{"user": "<id>"}` / `{"agent": "<id>"}`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActorRepr {
    User(UserId),
    Agent(UserId),
}

impl From<ActorRepr> for Actor {
    fn from(r: ActorRepr) -> Self {
        match r {
            ActorRepr::User(id) => Actor::User(id),
            ActorRepr::Agent(id) => Actor::Agent { on_behalf_of: id },
        }
    }
}

impl From<Actor> for ActorRepr {
    fn from(a: Actor) -> Self {
        match a {
            Actor::User(id) => ActorRepr::User(id),
            Actor::Agent { on_behalf_of } => ActorRepr::Agent(on_behalf_of),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Move,
    Rename,
    Trash,
    WriteEmbeddedTags,
    RelinkSidecar,
    WriteSidecar,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub op_id: OpId,
    pub plan_id: Option<PlanId>,
    pub actor: Actor,
    pub at: DateTime<Utc>,
    pub kind: OpKind,
    pub before: serde_json::Value,
    pub after: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new(library: &Library) -> Journal {
        Journal {
            dir: library.root().join(META_DIR).join(JOURNAL_DIR),
        }
    }

    /// Appends one line to the month file of `e.at` and syncs it to disk.
    pub fn append(&self, e: &JournalEntry) -> Result<()> {
        let mut line = serde_json::to_vec(e).map_err(std::io::Error::from)?;
        line.push(b'\n');
        fs::create_dir_all(&self.dir)?;
        let name = format!("{:04}-{:02}.{EXT}", e.at.year(), e.at.month());
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.dir.join(name))?;
        // One write_all of the whole line, so a crash can only leave a truncated tail.
        f.write_all(&line)?;
        f.sync_data()?;
        Ok(())
    }

    /// Entries in file-name order, then line order. A truncated final line of the last file
    /// (no trailing newline, unparsable) is skipped with a warning; any other malformed line is an
    /// error.
    pub fn read_all(&self) -> Result<Vec<JournalEntry>> {
        let mut files = match fs::read_dir(&self.dir) {
            Ok(rd) => rd
                .map(|r| r.map(|d| d.path()))
                .collect::<std::io::Result<Vec<_>>>()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        files.retain(|p| p.extension().is_some_and(|x| x == EXT));
        files.sort();
        let mut out = Vec::new();
        let last_file = files.len().saturating_sub(1);
        for (i, file) in files.iter().enumerate() {
            read_file(file, i == last_file, &mut out)?;
        }
        Ok(out)
    }
}

fn read_file(file: &Path, is_last_file: bool, out: &mut Vec<JournalEntry>) -> Result<()> {
    let bytes = fs::read(file)?;
    for (idx, raw) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
        let terminated = raw.ends_with(b"\n");
        let body = if terminated {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        match serde_json::from_slice::<JournalEntry>(body) {
            Ok(entry) => out.push(entry),
            Err(_) if !terminated && is_last_file => {
                tracing::warn!(
                    file = %file.display(),
                    line = idx + 1,
                    "skipping truncated last journal line"
                );
            }
            Err(_) => {
                return Err(Error::MalformedJournal {
                    file: file.to_path_buf(),
                    line: idx + 1,
                });
            }
        }
    }
    Ok(())
}
