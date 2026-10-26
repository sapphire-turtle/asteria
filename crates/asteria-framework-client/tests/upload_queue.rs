use std::fs;
use std::path::PathBuf;

use asteria_framework_api::HashRef;
use asteria_framework_client::upload::queue::{GroupId, GroupState, UploadQueue, queue_path};
use asteria_framework_client::upload::{DropPlan, PlannedFile, PlannedGroup};

fn plan(names: &[&str]) -> DropPlan {
    DropPlan {
        groups: names
            .iter()
            .map(|n| PlannedGroup {
                files: vec![PlannedFile {
                    local: PathBuf::from(format!("/home/u/{n}")),
                    inbox_path: format!("旅行/{n}"),
                }],
            })
            .collect(),
        ..DropPlan::default()
    }
}

fn ids(q: &UploadQueue) -> Vec<GroupId> {
    q.groups.iter().map(|g| g.id).collect()
}

#[test]
fn roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("uploads").join("lib.json");
    let mut q = UploadQueue::default();
    q.push(plan(&["a.jpg", "b.jpg"]));
    assert!(q.groups.iter().all(|g| g.state == GroupState::Hashing));
    let f = &mut q.groups[0].files[0];
    assert_eq!((f.size, f.mtime_ns), (0, 0));
    f.size = 10;
    f.mtime_ns = 1_700_000_000_123_456_789;
    f.file_hash = Some("blake3:ab".into());
    f.payload = Some(HashRef {
        hash: "blake3:cd".into(),
        scope: "payload".into(),
    });
    f.session = Some("s1".into());
    q.groups[1].state = GroupState::Done {
        placed: vec!["Inbox/旅行/b.jpg".into()],
        warnings: vec!["w".into()],
    };
    q.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(r#""version":1"#));
    assert!(text.contains(r#""state":"hashing""#), "{text}");
    assert!(text.contains(r#""state":"done","placed""#), "{text}");
    assert!(text.contains("旅行/a.jpg"));
    assert_eq!(UploadQueue::load(&path).unwrap(), q);
}

#[test]
fn unreadable_file_is_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.json");
    fs::write(&path, b"{ not json").unwrap();
    let q = UploadQueue::load(&path).unwrap();
    assert!(q.groups.is_empty());
    assert!(!path.exists());
    let aside: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        aside.iter().any(|n| n.starts_with("lib.json.bad-")),
        "{aside:?}"
    );
    assert!(
        UploadQueue::load(&dir.path().join("missing.json"))
            .unwrap()
            .groups
            .is_empty()
    );
}

#[test]
fn decide_keep_and_discard() {
    let mut q = UploadQueue::default();
    q.push(plan(&["a.jpg", "b.jpg", "c.jpg"]));
    let id = ids(&q);
    for g in &mut q.groups {
        g.state = GroupState::AwaitingDecision {
            existing: vec!["x".into()],
        };
    }
    assert!(q.decide(id[0], true));
    assert_eq!(
        q.groups[0].state,
        GroupState::Uploading {
            allow_duplicate: true
        }
    );
    assert!(!q.decide(id[0], false), "wrong state is a no-op");
    assert!(q.decide(id[1], false));
    assert_eq!(q.groups[1].state, GroupState::Discarded);
    assert!(!q.decide(GroupId(grain_id::GrainId::random()), true));
    assert!(q.decide_all(true));
    assert_eq!(
        q.groups[2].state,
        GroupState::Uploading {
            allow_duplicate: true
        }
    );
    assert_eq!(q.groups[1].state, GroupState::Discarded);
    assert!(!q.decide_all(true));
}

#[test]
fn clear_finished_keeps_pending() {
    let mut q = UploadQueue::default();
    q.push(plan(&["a", "b", "c", "d"]));
    q.groups[0].state = GroupState::Done {
        placed: vec![],
        warnings: vec![],
    };
    q.groups[1].state = GroupState::Discarded;
    q.groups[2].state = GroupState::Failed { reason: "r".into() };
    let keep = [q.groups[2].id, q.groups[3].id];
    q.clear_finished();
    assert_eq!(ids(&q), keep);
}

#[test]
fn retry_cancel_and_summary() {
    let mut q = UploadQueue::default();
    q.push(plan(&["a", "b", "c", "d"]));
    let id = ids(&q);
    q.groups[0].state = GroupState::Failed { reason: "r".into() };
    q.groups[0].sent = 5;
    assert!(q.retry(id[0]));
    assert_eq!(q.groups[0].state, GroupState::Hashing);
    assert_eq!(q.groups[0].sent, 0);
    assert!(!q.retry(id[0]));
    q.groups[1].state = GroupState::Uploading {
        allow_duplicate: false,
    };
    q.groups[1].files[0].size = 100;
    q.groups[1].sent = 40;
    q.groups[2].state = GroupState::AwaitingDecision { existing: vec![] };
    q.groups[3].state = GroupState::Done {
        placed: vec![],
        warnings: vec![],
    };
    let s = q.summary();
    assert_eq!((s.total, s.done, s.awaiting, s.failed), (4, 1, 1, 0));
    assert_eq!(s.active, Some((id[1], 40, 100)));
    assert!(q.cancel(id[1]));
    assert_eq!(q.groups[1].state, GroupState::Discarded);
    assert!(!q.cancel(id[1]));
    assert!(q.summary().active.is_none());
}

#[test]
fn group_id_text_roundtrip() {
    let id = GroupId(grain_id::GrainId::random());
    assert_eq!(id.to_string().parse::<GroupId>().unwrap(), id);
}

#[test]
fn unsafe_library_id_has_no_queue_path() {
    let root = std::path::Path::new("/data");
    assert_eq!(queue_path(root, "../x"), None);
    assert_eq!(queue_path(root, ""), None);
    assert_eq!(
        queue_path(root, "abc-1"),
        Some(root.join("uploads").join("abc-1.json"))
    );
}

#[test]
fn unknown_version_is_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.json");
    fs::write(&path, br#"{"version":2,"groups":[]}"#).unwrap();
    assert!(UploadQueue::load(&path).unwrap().groups.is_empty());
    assert!(!path.exists());
    let aside = fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(aside, 1);
}

#[test]
fn other_read_errors_are_returned() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where the file should be: not NotFound.
    let path = dir.path().join("lib.json");
    fs::create_dir(&path).unwrap();
    assert!(UploadQueue::load(&path).is_err());
    assert!(path.exists());
}

#[test]
fn cancel_does_nothing_on_done() {
    let mut q = UploadQueue::default();
    q.push(plan(&["a"]));
    let done = GroupState::Done {
        placed: vec!["p".into()],
        warnings: vec![],
    };
    q.groups[0].state = done.clone();
    let id = q.groups[0].id;
    assert!(!q.cancel(id));
    assert_eq!(q.groups[0].state, done);
}
