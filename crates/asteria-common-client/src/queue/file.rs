//! The queue file: one JSON line per event, `add` for a rating and `done` once it is settled.
//! Appending is cheap and a crash loses at most a torn last line; the file is rewritten
//! (compacted) when it holds nothing pending or many `done` lines.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use asteria_common_api::{ItemId, OpKind};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;

/// More `done` lines than this and the file is rewritten without them.
const COMPACT_AFTER: usize = 1000;

/// A rating waiting to be sent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueuedRating {
    pub id: String,
    pub item: ItemId,
    pub value: i8,
    pub at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Line {
    Add(AddLine),
    Done(String),
}

#[derive(Serialize, Deserialize)]
struct AddLine {
    id: String,
    kind: OpKind,
    item: ItemId,
    value: i8,
    at: String,
}

impl From<&QueuedRating> for Line {
    fn from(op: &QueuedRating) -> Line {
        Line::Add(AddLine {
            id: op.id.clone(),
            kind: OpKind::Rating,
            item: op.item.clone(),
            value: op.value,
            at: op.at.to_rfc3339_opts(SecondsFormat::Millis, true),
        })
    }
}

fn encode(line: &Line) -> String {
    // A plain enum of strings and numbers cannot fail to serialize.
    let mut s = serde_json::to_string(line).expect("queue line serializes");
    s.push('\n');
    s
}

fn decode(text: &str) -> Option<Line> {
    let line: Line = serde_json::from_str(text).ok()?;
    if let Line::Add(add) = &line {
        DateTime::parse_from_rfc3339(&add.at).ok()?;
    }
    Some(line)
}

/// The ratings of one library and profile still to send, backed by a JSONL file.
pub(crate) struct QueueFile {
    path: PathBuf,
    pending: Vec<QueuedRating>,
    /// `done` lines in the file.
    done_lines: usize,
    /// The file does not end in a complete line, so an append would glue onto it.
    torn: bool,
}

impl QueueFile {
    /// Missing file → empty. A torn last line is dropped; an unreadable middle line is skipped
    /// with a `tracing::warn!`; the file is never deleted or moved. Other read errors are returned.
    pub fn open(path: &Path) -> io::Result<QueueFile> {
        let mut q = QueueFile {
            path: path.to_owned(),
            pending: Vec::new(),
            done_lines: 0,
            torn: false,
        };
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(q),
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&bytes);
        q.torn = !text.is_empty() && !text.ends_with('\n');
        let all: Vec<&str> = text.lines().collect();
        for (n, raw) in all.iter().enumerate() {
            if raw.trim().is_empty() {
                continue;
            }
            match decode(raw) {
                Some(Line::Add(add)) => {
                    let at = DateTime::parse_from_rfc3339(&add.at)
                        .expect("checked by decode")
                        .with_timezone(&Utc);
                    q.pending.push(QueuedRating {
                        id: add.id,
                        item: add.item,
                        value: add.value,
                        at,
                    });
                }
                Some(Line::Done(id)) => {
                    q.done_lines += 1;
                    q.pending.retain(|p| p.id != id);
                }
                None if n + 1 == all.len() => {
                    // A write cut short by a crash.
                    q.torn = true;
                }
                None => tracing::warn!(
                    "skipping an unreadable line {} of {}",
                    n + 1,
                    path.display()
                ),
            }
        }
        Ok(q)
    }

    /// The ratings not yet settled, oldest line first.
    pub fn pending(&self) -> &[QueuedRating] {
        &self.pending
    }

    /// Appends one `add` line (creating parent dirs), then records it as pending.
    pub fn append(&mut self, op: QueuedRating) -> io::Result<()> {
        if self.torn {
            // Rewrites the file without the partial line before anything follows it.
            self.compact()?;
        }
        self.write_lines(&encode(&Line::from(&op)))?;
        self.pending.push(op);
        Ok(())
    }

    /// Appends one `done` line per id still pending, drops them, then compacts.
    pub fn mark_done(&mut self, ids: &[String]) -> io::Result<()> {
        let ids: Vec<&String> = ids
            .iter()
            .filter(|id| self.pending.iter().any(|p| &p.id == *id))
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        if self.torn {
            self.compact()?;
        }
        let text: String = ids
            .iter()
            .map(|id| encode(&Line::Done((*id).clone())))
            .collect();
        self.write_lines(&text)?;
        self.done_lines += ids.len();
        self.pending.retain(|p| !ids.contains(&&p.id));
        if self.pending.is_empty() || self.done_lines > COMPACT_AFTER {
            self.compact()?;
        }
        Ok(())
    }

    /// The newest pending rating of each item (greatest `at`; on a tie the later line), in the
    /// order the items first appear; and the ids of the older ones, which can be marked done unsent.
    pub fn coalesced(&self) -> (Vec<QueuedRating>, Vec<String>) {
        let mut slot: HashMap<&ItemId, usize> = HashMap::new();
        let mut send: Vec<&QueuedRating> = Vec::new();
        let mut obsolete = Vec::new();
        for op in &self.pending {
            match slot.get(&op.item) {
                None => {
                    slot.insert(&op.item, send.len());
                    send.push(op);
                }
                Some(&i) if op.at >= send[i].at => {
                    obsolete.push(send[i].id.clone());
                    send[i] = op;
                }
                Some(_) => obsolete.push(op.id.clone()),
            }
        }
        // Ids go in line order, whichever of the two rated first.
        let order: HashMap<&str, usize> = self
            .pending
            .iter()
            .enumerate()
            .map(|(n, p)| (p.id.as_str(), n))
            .collect();
        obsolete.sort_by_key(|id| order.get(id.as_str()).copied());
        (send.into_iter().cloned().collect(), obsolete)
    }

    fn write_lines(&self, text: &str) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(text.as_bytes())?;
        f.sync_data()
    }

    /// Rewrites the file as the pending `add` lines alone.
    fn compact(&mut self) -> io::Result<()> {
        let text: String = self
            .pending
            .iter()
            .map(|op| encode(&Line::from(op)))
            .collect();
        write_atomic(&self.path, text.as_bytes(), false)?;
        self.done_lines = 0;
        self.torn = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::queue_path;
    use chrono::TimeZone;
    use std::fs;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 16, h, 0, 0).unwrap()
    }

    fn op(id: &str, item: &str, value: i8, h: u32) -> QueuedRating {
        QueuedRating {
            id: id.into(),
            item: ItemId(item.into()),
            value,
            at: at(h),
        }
    }

    fn lines(path: &Path) -> usize {
        fs::read_to_string(path).unwrap().lines().count()
    }

    #[test]
    fn append_and_reopen_keeps_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q/lib/p.jsonl");
        let mut q = QueueFile::open(&path).unwrap();
        q.append(op("a", "i1", 3, 1)).unwrap();
        q.append(op("b", "i2", -1, 2)).unwrap();
        let q = QueueFile::open(&path).unwrap();
        assert_eq!(q.pending(), [op("a", "i1", 3, 1), op("b", "i2", -1, 2)]);
    }

    #[test]
    fn done_lines_remove_ops_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.jsonl");
        let mut q = QueueFile::open(&path).unwrap();
        q.append(op("a", "i1", 3, 1)).unwrap();
        q.append(op("b", "i2", 4, 2)).unwrap();
        q.mark_done(&["a".into()]).unwrap();
        assert_eq!(q.pending(), [op("b", "i2", 4, 2)]);
        let q = QueueFile::open(&path).unwrap();
        assert_eq!(q.pending(), [op("b", "i2", 4, 2)]);
    }

    #[test]
    fn the_file_empties_when_nothing_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.jsonl");
        let mut q = QueueFile::open(&path).unwrap();
        q.append(op("a", "i1", 3, 1)).unwrap();
        q.mark_done(&["a".into()]).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn many_done_lines_are_compacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.jsonl");
        let mut q = QueueFile::open(&path).unwrap();
        for n in 0..1002 {
            q.append(op(&format!("o{n}"), "i1", 1, 1)).unwrap();
        }
        for n in 0..1001 {
            q.mark_done(&[format!("o{n}")]).unwrap();
        }
        assert_eq!(lines(&path), 1);
        let q = QueueFile::open(&path).unwrap();
        assert_eq!(q.pending(), [op("o1001", "i1", 1, 1)]);
    }

    #[test]
    fn a_torn_last_line_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.jsonl");
        let mut q = QueueFile::open(&path).unwrap();
        q.append(op("a", "i1", 3, 1)).unwrap();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(r#"{"add":{"id":"x""#);
        fs::write(&path, &text).unwrap();
        let mut q = QueueFile::open(&path).unwrap();
        assert_eq!(q.pending(), [op("a", "i1", 3, 1)]);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        // The next line does not glue onto the torn one.
        q.append(op("b", "i2", 2, 2)).unwrap();
        let q = QueueFile::open(&path).unwrap();
        assert_eq!(q.pending(), [op("a", "i1", 3, 1), op("b", "i2", 2, 2)]);
    }

    #[test]
    fn an_unreadable_middle_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.jsonl");
        let mut q = QueueFile::open(&path).unwrap();
        q.append(op("a", "i1", 3, 1)).unwrap();
        let good = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{good}garbage\n{good}")).unwrap();
        let q = QueueFile::open(&path).unwrap();
        assert_eq!(q.pending().len(), 2);
    }

    #[test]
    fn coalesced_keeps_the_newest_per_item() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = QueueFile::open(&dir.path().join("p.jsonl")).unwrap();
        q.append(op("1", "A", 1, 10)).unwrap();
        q.append(op("2", "B", 2, 10)).unwrap();
        q.append(op("3", "A", 3, 11)).unwrap();
        q.append(op("4", "A", 4, 9)).unwrap();
        let (send, obsolete) = q.coalesced();
        assert_eq!(send, [op("3", "A", 3, 11), op("2", "B", 2, 10)]);
        assert_eq!(obsolete, ["1", "4"]);
    }

    #[test]
    fn queue_path_rejects_unsafe_ids() {
        let root = Path::new("/data");
        assert_eq!(queue_path(root, "../x", "p1"), None);
        assert_eq!(
            queue_path(root, "lib1", "p1"),
            Some(root.join("queue/lib1/p1.jsonl"))
        );
    }
}
