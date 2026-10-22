use std::fs;

use asteria_framework::journal::{Actor, Journal, JournalEntry, OpId, OpKind, PlanId};
use asteria_framework::{Error, Library};
use chrono::{TimeZone, Utc};
use grain_id::GrainId;
use serde_json::json;

fn entry(lib: &Library, at: chrono::DateTime<Utc>, before: serde_json::Value) -> JournalEntry {
    JournalEntry {
        op_id: OpId(GrainId::random()),
        plan_id: Some(PlanId(GrainId::random())),
        actor: Actor::User(lib.owner()),
        at,
        kind: OpKind::Move,
        before,
        after: json!({"path": "b.jpg"}),
    }
}

fn setup() -> (tempfile::TempDir, Library, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::init(dir.path(), "fluo10").unwrap();
    let j = Journal::new(&lib);
    (dir, lib, j)
}

fn day(m: u32, d: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, m, d, 12, 0, 0).unwrap()
}

fn jfile(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(".asteria/journal").join(name)
}

#[test]
fn append_and_read_back_in_order() {
    let (_d, lib, j) = setup();
    let es: Vec<_> = (1..=3)
        .map(|i| entry(&lib, day(10, i), json!({"i": i})))
        .collect();
    for e in &es {
        j.append(e).unwrap();
    }
    assert_eq!(j.read_all().unwrap(), es);
}

#[test]
fn read_all_without_journal_dir_is_empty() {
    let (_d, _lib, j) = setup();
    assert!(j.read_all().unwrap().is_empty());
}

#[test]
fn entries_split_by_month() {
    let (d, lib, j) = setup();
    let nov = entry(
        &lib,
        Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(),
        json!(1),
    );
    let oct = entry(
        &lib,
        Utc.with_ymd_and_hms(2026, 10, 31, 23, 59, 59).unwrap(),
        json!(2),
    );
    j.append(&nov).unwrap();
    j.append(&oct).unwrap();
    assert!(jfile(&d, "2026-10.jsonl").is_file());
    assert!(jfile(&d, "2026-11.jsonl").is_file());
    assert_eq!(j.read_all().unwrap(), vec![oct, nov]);
}

#[test]
fn japanese_paths_survive_roundtrip() {
    let (_d, lib, j) = setup();
    let e = entry(&lib, day(10, 1), json!({"path": "2025/京都旅行/a.jpg"}));
    j.append(&e).unwrap();
    assert_eq!(j.read_all().unwrap(), vec![e]);
}

#[test]
fn actor_serde_shape() {
    let (_d, lib, _j) = setup();
    let id = lib.owner();
    let u = serde_json::to_value(Actor::User(id)).unwrap();
    let a = serde_json::to_value(Actor::Agent { on_behalf_of: id }).unwrap();
    assert_eq!(u, json!({"user": id.to_string()}));
    assert_eq!(a, json!({"agent": id.to_string()}));
    assert_eq!(
        serde_json::from_value::<Actor>(a).unwrap(),
        Actor::Agent { on_behalf_of: id }
    );
    assert_eq!(
        serde_json::to_value(OpKind::WriteEmbeddedTags).unwrap(),
        json!("write_embedded_tags")
    );
}

#[test]
fn truncated_last_line_is_skipped() {
    let (d, lib, j) = setup();
    let es: Vec<_> = (1..=3).map(|i| entry(&lib, day(10, i), json!(i))).collect();
    for e in &es {
        j.append(e).unwrap();
    }
    let p = jfile(&d, "2026-10.jsonl");
    let bytes = fs::read(&p).unwrap();
    fs::write(&p, &bytes[..bytes.len() - 20]).unwrap();
    assert_eq!(j.read_all().unwrap(), es[..2]);
}

#[test]
fn corrupt_middle_line_is_error() {
    let (d, lib, j) = setup();
    for i in 1..=3 {
        j.append(&entry(&lib, day(10, i), json!(i))).unwrap();
    }
    let p = jfile(&d, "2026-10.jsonl");
    let text = fs::read_to_string(&p).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines[1] = "{not json";
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    match j.read_all() {
        Err(Error::MalformedJournal { file, line }) => {
            assert_eq!(file, p);
            assert_eq!(line, 2);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn corrupt_newline_terminated_last_line_is_error() {
    let (d, lib, j) = setup();
    j.append(&entry(&lib, day(10, 1), json!(1))).unwrap();
    let p = jfile(&d, "2026-10.jsonl");
    let mut text = fs::read_to_string(&p).unwrap();
    text.push_str("{broken\n");
    fs::write(&p, text).unwrap();
    assert!(matches!(
        j.read_all(),
        Err(Error::MalformedJournal { line: 2, .. })
    ));
}

#[test]
fn unterminated_fragment_in_earlier_file_is_skipped_but_terminated_corrupt_is_error() {
    let (d, lib, j) = setup();
    let e1 = entry(&lib, day(10, 1), json!(1));
    let e2 = entry(&lib, day(11, 1), json!(2));
    j.append(&e1).unwrap();
    j.append(&e1).unwrap();
    j.append(&e2).unwrap();
    let p = jfile(&d, "2026-10.jsonl");
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

#[test]
fn append_after_torn_tail_preserves_fragment_and_stays_readable() {
    let (d, lib, j) = setup();
    let es: Vec<_> = (1..=3).map(|i| entry(&lib, day(10, i), json!(i))).collect();
    j.append(&es[0]).unwrap();
    j.append(&es[1]).unwrap();
    let p = jfile(&d, "2026-10.jsonl");
    let bytes = fs::read(&p).unwrap();
    let cut = bytes.len() - 15;
    fs::write(&p, &bytes[..cut]).unwrap();
    let first_len = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
    j.append(&es[2]).unwrap();
    assert_eq!(j.read_all().unwrap(), vec![es[0].clone(), es[2].clone()]);
    let torn = fs::read(jfile(&d, "2026-10.jsonl.torn")).unwrap();
    assert!(torn.starts_with(&bytes[first_len..cut]));
}

#[test]
fn plan_id_none_roundtrips() {
    let (_d, lib, j) = setup();
    let mut e = entry(&lib, day(10, 1), json!(1));
    e.plan_id = None;
    e.actor = Actor::Agent {
        on_behalf_of: lib.owner(),
    };
    j.append(&e).unwrap();
    assert_eq!(j.read_all().unwrap(), vec![e]);
}
