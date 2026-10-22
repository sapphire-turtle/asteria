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
    ///
    /// If the file ends in a torn (unterminated) fragment from a crash, the fragment is first
    /// preserved in `<file>.torn` and cut off, so the new line starts on a clean boundary.
    pub fn append(&self, e: &JournalEntry) -> Result<()> {
        let mut line =
            serde_json::to_vec(e).map_err(|err| Error::JournalSerialize(err.to_string()))?;
        line.push(b'\n');
        fs::create_dir_all(&self.dir)?;
        let name = format!("{:04}-{:02}.{EXT}", e.at.year(), e.at.month());
        let path = self.dir.join(name);
        let is_new = !path.exists();
        if !is_new {
            repair_torn_tail(&path)?;
        }
        let mut f = OpenOptions::new().append(true).create(true).open(&path)?;
        // One write_all of the whole line, so a crash can only leave a truncated tail.
        f.write_all(&line)?;
        f.sync_data()?;
        if is_new {
            sync_dir(&self.dir)?;
        }
        Ok(())
    }

    /// Entries in file-name order, then line order. An unterminated, unparsable final fragment of
    /// any file is skipped with a warning; a newline-terminated malformed line is an error.
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
        for file in &files {
            read_file(file, &mut out)?;
        }
        Ok(out)
    }
}

fn read_file(file: &Path, out: &mut Vec<JournalEntry>) -> Result<()> {
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
            Err(_) if !terminated => {
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

fn repair_torn_tail(path: &Path) -> Result<()> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut torn_name = path.as_os_str().to_owned();
    torn_name.push(".torn");
    let mut torn = OpenOptions::new()
        .append(true)
        .create(true)
        .open(PathBuf::from(torn_name))?;
    torn.write_all(&bytes[keep..])?;
    torn.write_all(b"\n")?;
    torn.sync_data()?;
    tracing::warn!(file = %path.display(), "cut torn journal tail before appending");
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_len(keep as u64)?;
    f.sync_data()?;
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}
