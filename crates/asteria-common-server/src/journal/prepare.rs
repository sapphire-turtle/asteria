//! Makes the journal directory ready before the store opens: an old-format directory moves
//! aside, and month files past the undo window are removed. Only moves and deletes; no store
//! is opened here.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a change stays undoable, and so how long its month file is kept.
pub const UNDO_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// corundum's lock file; its presence marks a directory in corundum's journal format.
const LOCK_FILE: &str = "journal.lock";

/// What `prepare` did to `dir` (the journal directory) before the store opens.
pub struct Prepared {
    pub moved_to: Option<PathBuf>,
    pub removed: Vec<PathBuf>,
}

/// Moves an old-format `dir` (no `journal.lock` but some `*.jsonl`) aside to the first free
/// `journal-v1`, `journal-v1-2`, … beside it, then removes the month files of `dir` whose
/// last day ends more than `keep` before `now_ms`. A missing `dir` is fine (nothing to do).
pub fn prepare(dir: &Path, now_ms: i64, keep: Duration) -> io::Result<Prepared> {
    let mut prepared = Prepared {
        moved_to: None,
        removed: Vec::new(),
    };
    if !dir.is_dir() {
        return Ok(prepared);
    }
    if !dir.join(LOCK_FILE).exists() && has_jsonl(dir)? {
        let to = first_free_beside(dir);
        fs::rename(dir, &to)?;
        tracing::warn!(
            from = %dir.display(),
            to = %to.display(),
            "old-format journal moved aside"
        );
        prepared.moved_to = Some(to);
        return Ok(prepared);
    }
    let keep_ms = i64::try_from(keep.as_millis()).unwrap_or(i64::MAX);
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some((year, month)) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(month_of_file)
        else {
            continue;
        };
        if month_end_ms(year, month).saturating_add(keep_ms) < now_ms {
            fs::remove_file(&path)?;
            tracing::info!(path = %path.display(), "journal month past the undo window removed");
            prepared.removed.push(path);
        }
    }
    prepared.removed.sort();
    Ok(prepared)
}

fn has_jsonl(dir: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        if entry?.path().extension().is_some_and(|e| e == "jsonl") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `journal-v1`, then `journal-v1-2`, `journal-v1-3`, … next to `dir`, the first not taken.
fn first_free_beside(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "journal".into());
    let parent = dir.parent().unwrap_or_else(|| Path::new(""));
    let first = parent.join(format!("{name}-v1"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| parent.join(format!("{name}-v1-{n}")))
        .find(|p| !p.exists())
        .expect("an unbounded range has a free name")
}

/// `(year, month)` of a `YYYY-MM.jsonl` file name.
fn month_of_file(name: &str) -> Option<(i64, u32)> {
    let stem = name.strip_suffix(".jsonl")?;
    let (year, month) = stem.split_once('-')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    let (year, month) = (year.parse().ok()?, month.parse().ok()?);
    (1..=12).contains(&month).then_some((year, month))
}

/// Milliseconds since the Unix epoch at which the month ends (UTC): the next month's start.
fn month_end_ms(year: i64, month: u32) -> i64 {
    let (y, m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    days_from_civil(y, m, 1) * 86_400_000
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_days_match_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 11, 19), 20_776);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn only_month_files_are_recognised() {
        assert_eq!(month_of_file("2026-10.jsonl"), Some((2026, 10)));
        assert_eq!(month_of_file("2026-13.jsonl"), None);
        assert_eq!(month_of_file("notes.txt"), None);
        assert_eq!(month_of_file("2026-1.jsonl"), None);
    }
}
