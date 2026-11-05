//! Append-only JSONL journal of file operations, one `YYYY-MM.jsonl` file per month (UTC) of
//! the append.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::UserId;
/// The journal's directory under an instance's state directory.
pub(crate) const JOURNAL_DIR: &str = "journal";
const EXT: &str = "jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpId(pub GrainId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BatchId(pub GrainId);

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
    Upload,
    Batch,
    BatchEnd,
    RewritePlaylist,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub op_id: OpId,
    pub plan_id: Option<BatchId>,
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
    /// The journal kept in `dir`. It belongs to one server instance, so `dir` is in that
    /// instance's state directory, never in the shared workspace (see [`Library::journal`]).
    ///
    /// [`Library::journal`]: crate::Library::journal
    pub fn new(dir: impl Into<PathBuf>) -> Journal {
        Journal { dir: dir.into() }
    }

    /// Appends one line to the month file (UTC) of the current time and syncs it to disk.
    ///
    /// The file is chosen by when the append happens, not by `e.at`, so file order equals append
    /// order; `e.at` is stored as given.
    ///
    /// If the file ends in a torn (unterminated) fragment from a crash, the fragment is first
    /// preserved in `<file>.torn` and cut off, so the new line starts on a clean boundary.
    pub fn append(&self, e: &JournalEntry) -> Result<()> {
        self.append_at(e, Utc::now())
    }

    /// [`Journal::append`] with the clock injected; `now` picks the month file.
    pub(crate) fn append_at(&self, e: &JournalEntry, now: DateTime<Utc>) -> Result<()> {
        let mut line =
            serde_json::to_vec(e).map_err(|err| Error::JournalSerialize(err.to_string()))?;
        line.push(b'\n');
        fs::create_dir_all(&self.dir)?;
        let name = format!("{:04}-{:02}.{EXT}", now.year(), now.month());
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
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    f.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    // Torn: scan backwards in chunks for the last newline.
    let mut keep = 0u64;
    let mut end = len;
    'scan: while end > 0 {
        let start = end.saturating_sub(4096);
        let mut buf = vec![0u8; (end - start) as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut buf)?;
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            keep = start + i as u64 + 1;
            break 'scan;
        }
        end = start;
    }
    let mut tail = vec![0u8; (len - keep) as usize];
    f.seek(SeekFrom::Start(keep))?;
    f.read_exact(&mut tail)?;
    if serde_json::from_slice::<JournalEntry>(&tail).is_ok() {
        // A complete entry that only lost its newline: keep it.
        f.seek(SeekFrom::End(0))?;
        f.write_all(b"\n")?;
        f.sync_data()?;
        return Ok(());
    }
    let mut torn_name = path.as_os_str().to_owned();
    torn_name.push(".torn");
    let mut torn = OpenOptions::new()
        .append(true)
        .create(true)
        .open(PathBuf::from(torn_name))?;
    torn.write_all(&tail)?;
    torn.write_all(b"\n")?;
    torn.sync_data()?;
    tracing::warn!(file = %path.display(), "cut torn journal tail before appending");
    f.set_len(keep)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn journal() -> (tempfile::TempDir, Journal) {
        let d = tempfile::tempdir().unwrap();
        let j = Journal {
            dir: d.path().join(JOURNAL_DIR),
        };
        (d, j)
    }

    fn entry(at: DateTime<Utc>, n: i32) -> JournalEntry {
        let user = UserId(GrainId::random());
        JournalEntry {
            op_id: OpId(GrainId::random()),
            plan_id: None,
            actor: Actor::User(user),
            at,
            kind: OpKind::Move,
            before: json!(n),
            after: json!(n),
        }
    }

    fn ymd(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, s).unwrap()
    }

    #[test]
    fn entries_split_by_append_month() {
        let (_d, j) = journal();
        let oct_end = ymd(2026, 10, 31, 23, 59, 59);
        let nov_start = ymd(2026, 11, 1, 0, 0, 0);
        // An entry stamped in November but appended in October, then the reverse.
        let a = entry(nov_start, 1);
        let b = entry(oct_end, 2);
        j.append_at(&a, oct_end).unwrap();
        j.append_at(&b, nov_start).unwrap();
        let read = |name: &str| {
            let mut out = Vec::new();
            read_file(&j.dir.join(name), &mut out).unwrap();
            out
        };
        assert_eq!(read("2026-10.jsonl"), vec![a.clone()]);
        assert_eq!(read("2026-11.jsonl"), vec![b.clone()]);
        // File order equals append order; `at` is kept as given.
        assert_eq!(j.read_all().unwrap(), vec![a, b]);
    }

    #[test]
    fn unterminated_fragment_in_earlier_file_is_skipped_but_terminated_corrupt_is_error() {
        let (_d, j) = journal();
        let (oct, nov) = (ymd(2026, 10, 1, 12, 0, 0), ymd(2026, 11, 1, 12, 0, 0));
        let e1 = entry(oct, 1);
        let e2 = entry(nov, 2);
        j.append_at(&e1, oct).unwrap();
        j.append_at(&e1, oct).unwrap();
        j.append_at(&e2, nov).unwrap();
        let p = j.dir.join("2026-10.jsonl");
        let bytes = fs::read(&p).unwrap();
        fs::write(&p, &bytes[..bytes.len() - 10]).unwrap();
        assert_eq!(j.read_all().unwrap(), vec![e1, e2]);
        let mut b = fs::read(&p).unwrap();
        b.push(b'\n');
        fs::write(&p, b).unwrap();
        assert!(matches!(
            j.read_all(),
            Err(Error::MalformedJournal { line: 2, .. })
        ));
    }
}
