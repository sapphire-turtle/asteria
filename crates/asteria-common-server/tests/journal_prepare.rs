//! `journal::prepare`: the journal directory made ready before the store opens.

use std::fs;
use std::path::Path;
use std::time::Duration;

use asteria_common_server::journal::{UNDO_WINDOW, prepare};

/// 2026-11-19T00:00:00Z: day 20776 since the epoch.
const NOW_MS: i64 = 20_776 * 86_400_000;
const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

fn touch(dir: &Path, name: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), b"{}\n").unwrap();
}

#[test]
fn an_old_journal_moves_to_v1() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("journal");
    touch(&dir, "2026-10.jsonl");

    let prepared = prepare(&dir, NOW_MS, KEEP).unwrap();

    let v1 = tmp.path().join("journal-v1");
    assert_eq!(prepared.moved_to, Some(v1.clone()));
    assert!(v1.join("2026-10.jsonl").is_file());
    assert!(!dir.exists());
}

#[test]
fn a_second_old_journal_gets_a_number() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("journal");
    touch(&tmp.path().join("journal-v1"), "2026-01.jsonl");
    touch(&dir, "2026-10.jsonl");

    let prepared = prepare(&dir, NOW_MS, KEEP).unwrap();

    let v1_2 = tmp.path().join("journal-v1-2");
    assert_eq!(prepared.moved_to, Some(v1_2.clone()));
    assert!(v1_2.join("2026-10.jsonl").is_file());
    assert!(tmp.path().join("journal-v1/2026-01.jsonl").is_file());
}

#[test]
fn a_corundum_journal_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("journal");
    touch(&dir, "journal.lock");
    touch(&dir, "2026-11.jsonl");

    let prepared = prepare(&dir, NOW_MS, KEEP).unwrap();

    assert_eq!(prepared.moved_to, None);
    assert!(dir.join("journal.lock").is_file());
    assert!(dir.join("2026-11.jsonl").is_file());
}

#[test]
fn old_months_are_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("journal");
    touch(&dir, "journal.lock");
    for name in [
        "2026-09.jsonl",
        "2026-10.jsonl",
        "2026-11.jsonl",
        "notes.txt",
    ] {
        touch(&dir, name);
    }

    let prepared = prepare(&dir, NOW_MS, KEEP).unwrap();

    assert_eq!(prepared.removed, vec![dir.join("2026-09.jsonl")]);
    assert!(!dir.join("2026-09.jsonl").exists());
    assert!(dir.join("2026-10.jsonl").is_file());
    assert!(dir.join("2026-11.jsonl").is_file());
    assert!(dir.join("notes.txt").is_file());
}

#[test]
fn a_missing_dir_is_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let prepared = prepare(&tmp.path().join("journal"), NOW_MS, UNDO_WINDOW).unwrap();
    assert_eq!(prepared.moved_to, None);
    assert!(prepared.removed.is_empty());
}
