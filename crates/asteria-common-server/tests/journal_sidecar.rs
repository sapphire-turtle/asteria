//! The sidecar operations as corundum Journal operations: `write_sidecar` (`media.update`'s
//! tags, description and label) and `rate`, written through the Writer and undone.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use asteria_common_api::{ItemId, MediaUpdateParams, OpKind as RatingKind, RatingOp};
use asteria_common_core::journal::Actor as CoreActor;
use asteria_common_core::{Library, MediaKind};
use asteria_common_server::journal::sidecar::{Rate, RateDone, RatePlan, RatingAt};
use asteria_common_server::journal::{JournalCtx, ServerJournal, open_journal};
use asteria_common_server::{AppExtension, Cache, Writer, scan, spawn_writer};
use corundum_framework::journal::{
    Actor, BatchId, BatchRecords, OpKind as _, Origin, Outcome, Skip,
};

struct Photos;
impl AppExtension for Photos {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    lib: Arc<Library>,
    cache: Arc<Cache>,
    journal: ServerJournal,
    writer: Writer,
}

/// `a.jpg` with a sidecar (label `old`, tag `x`), scanned.
fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Arc::new(
        Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state")),
    );
    let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
    fs::write(root.join("a.jpg"), "photo a").unwrap();
    lib.update_sidecar(Path::new("a.jpg"), |sc| {
        sc.subjects.insert("x".into());
        sc.label = Some("old".into());
    })
    .unwrap();
    scan(&lib, &cache, &Photos).unwrap();
    let journal = open_journal(
        JournalCtx {
            library: lib.clone(),
            cache: cache.clone(),
            ext: Arc::new(Photos),
        },
        None,
    )
    .unwrap();
    let (writer, _h) = spawn_writer(lib.clone(), cache.clone(), journal.clone());
    Env {
        _dir: dir,
        root,
        lib,
        cache,
        journal,
        writer,
    }
}

impl Env {
    fn me(&self) -> CoreActor {
        CoreActor::User(self.lib.owner())
    }

    fn owner(&self) -> Actor {
        Actor::person(self.lib.owner().to_string())
    }

    fn id(&self) -> ItemId {
        self.cache.get_by_path("a.jpg").unwrap().unwrap().id
    }

    /// The newest batch.
    fn last(&self) -> BatchRecords {
        let newest = self.journal.history(None, 1).unwrap();
        self.journal.status(newest[0].begin.batch_id).unwrap()
    }

    fn undo(&self, id: BatchId) -> BatchRecords {
        let u = self
            .journal
            .undo_batch(id, &Origin::new("test"), &self.owner())
            .unwrap();
        self.journal.wait(u).unwrap()
    }

    /// Rates `a.jpg` at `at` (RFC 3339) with `ops.apply`; the batch it ran.
    async fn rate(&self, value: i8, at: &str) -> BatchRecords {
        let op = RatingOp {
            id: "o".into(),
            kind: RatingKind::Rating,
            item: self.id(),
            value,
            at: at.into(),
        };
        self.writer
            .apply_ops(self.me(), Origin::new("test"), vec![op])
            .await
            .unwrap();
        self.last()
    }

    fn rating(&self) -> Option<i8> {
        let sc = self.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
        sc.rating.map(|r| r.value)
    }
}

fn params(id: ItemId) -> MediaUpdateParams {
    MediaUpdateParams {
        id,
        rating: None,
        subjects: None,
        hierarchical_subjects: None,
        description: None,
        label: None,
    }
}

#[tokio::test]
async fn metadata_edit_undoes_to_the_old_values() {
    let e = env();
    let mut p = params(e.id());
    p.subjects = Some(vec!["y".into(), "z".into()]);
    p.description = Some(Some("about".into()));
    p.label = Some(None);
    let rec = e
        .writer
        .update(e.me(), Origin::new("test"), p)
        .await
        .unwrap();
    assert_eq!(rec.sidecar.subjects, ["y", "z"]);
    assert_eq!(rec.sidecar.label, None);

    let b = e.last();
    assert_eq!(b.summary.begin.label.as_deref(), Some("Edit metadata"));
    assert_eq!(b.summary.begin.actor, e.owner());
    let kinds: Vec<&str> = b.ops.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(kinds, ["write_sidecar"]);

    let undone = e.undo(b.summary.begin.batch_id);
    assert_eq!(undone.summary.done, 1, "{undone:?}");
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_eq!(sc.subjects.iter().collect::<Vec<_>>(), ["x"]);
    assert_eq!(sc.label.as_deref(), Some("old"));
    assert_eq!(sc.description, None);
    let cached = e.cache.get_by_path("a.jpg").unwrap().unwrap().sidecar;
    assert_eq!(cached.label.as_deref(), Some("old"));
    assert_eq!(cached.subjects, ["x"]);

    // Edited again since: the undo of the first edit leaves the new label alone.
    let mut p = params(e.id());
    p.label = Some(Some("new".into()));
    e.writer
        .update(e.me(), Origin::new("test"), p)
        .await
        .unwrap();
    let relabel = e.last().summary.begin.batch_id;
    let mut p = params(e.id());
    p.label = Some(Some("newer".into()));
    e.writer
        .update(e.me(), Origin::new("test"), p)
        .await
        .unwrap();
    let undone = e.undo(relabel);
    assert_eq!(undone.summary.skipped, 1, "{undone:?}");
    assert_eq!(
        undone.ops[0].outcome,
        Outcome::Skipped(Skip::PreconditionFailed)
    );
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_eq!(sc.label.as_deref(), Some("newer"));
}

#[tokio::test]
async fn a_rating_and_a_label_are_one_batch() {
    let e = env();
    let mut p = params(e.id());
    p.rating = Some(3);
    p.label = Some(Some("l".into()));
    let rec = e
        .writer
        .update(e.me(), Origin::new("test"), p)
        .await
        .unwrap();
    assert_eq!(rec.sidecar.owner_rating, Some(3));
    let b = e.last();
    let kinds: Vec<&str> = b.ops.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(kinds, ["write_sidecar", "rate"]);
    assert_eq!(e.journal.history(None, 10).unwrap().len(), 1);

    e.undo(b.summary.begin.batch_id);
    assert_eq!(e.rating(), None);
    let sc = e.lib.read_sidecar(Path::new("a.jpg")).unwrap().unwrap();
    assert_eq!(sc.label.as_deref(), Some("old"));
}

#[tokio::test]
async fn undo_skips_a_rating_changed_since() {
    let e = env();
    let first = e.rate(3, "2026-11-01T10:00:00Z").await;
    assert_eq!(first.summary.begin.label.as_deref(), Some("Rate 1 items"));
    assert_eq!(first.ops[0].kind, Rate::KIND);
    let later = e.rate(5, "2026-11-02T10:00:00Z").await;
    assert_eq!(e.rating(), Some(5));

    let undone = e.undo(first.summary.begin.batch_id);
    assert_eq!(
        undone.ops[0].outcome,
        Outcome::Skipped(Skip::PreconditionFailed),
        "{undone:?}"
    );
    assert_eq!(e.rating(), Some(5));

    // The later one undoes to the first.
    e.undo(later.summary.begin.batch_id);
    assert_eq!(e.rating(), Some(3));
    assert_eq!(
        e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .sidecar
            .owner_rating,
        Some(3)
    );
}

#[tokio::test]
async fn a_superseded_rating_undoes_to_nothing() {
    let e = env();
    e.rate(5, "2026-11-02T10:00:00Z").await;
    let older = e.rate(3, "2026-11-01T10:00:00Z").await;
    assert_eq!(e.rating(), Some(5));
    let Outcome::Done(done) = &older.ops[0].outcome else {
        panic!("{older:?}")
    };
    let done: RateDone = serde_json::from_value(done.clone()).unwrap();
    assert_eq!(done.before, done.after);
    assert_eq!(done.after.map(|r| r.value), Some(5));

    let undone = e.undo(older.summary.begin.batch_id);
    assert_eq!(undone.summary.done, 1, "{undone:?}");
    assert_eq!(undone.summary.skipped, 0);
    assert_eq!(e.rating(), Some(5));
}

#[test]
fn a_rate_plan_reads_as_a_rating_or_a_restore() {
    let user = asteria_common_core::UserId(grain_id::GrainId::random());
    let forward = RatePlan {
        path: "a.jpg".into(),
        user,
        rating: Some(RatingAt {
            value: 4,
            at_ms: Some(1_700_000_000_123),
        }),
        expect: None,
    };
    let v = serde_json::to_value(&forward).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "path": "a.jpg", "user": user.to_string(), "value": 4, "at_ms": 1_700_000_000_123i64,
        })
    );
    let restore = RatePlan {
        rating: None,
        expect: Some(Some(RatingAt {
            value: 4,
            at_ms: Some(1),
        })),
        ..forward.clone()
    };
    for plan in [forward, restore] {
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(serde_json::from_value::<RatePlan>(v).unwrap(), plan);
    }
}

#[tokio::test]
async fn a_deleted_file_is_not_found() {
    let e = env();
    fs::remove_file(e.root.join("a.jpg")).unwrap();
    let mut p = params(e.id());
    p.label = Some(Some("x".into()));
    let r = e.writer.update(e.me(), Origin::new("test"), p).await;
    assert_eq!(r, Err(asteria_common_server::WriteError::NotFound));
}
