//! The file operations as corundum Journal operations: `move`, `trash`, `organize_tidy` and
//! `rewrite_playlist`, run and undone over a temporary library.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use asteria_common_api::codes;
use asteria_common_core::{Library, MediaKind};
use asteria_common_server::journal::files::{
    Expect, Move, MoveDone, MovePlan, OrganizeTidy, TidyMove, TidyPlan, Trash, TrashPlan,
};
use asteria_common_server::journal::map::rpc_error;
use asteria_common_server::journal::{JournalCtx, ServerJournal, open_journal};
use asteria_common_server::{
    AppExtension, Cache, ExtContext, Rewrite, hash_pending, item_id_for, scan,
};
use corundum_framework::journal::{
    Actor, BatchId, BatchRecords, JournalError, OpRecord, Origin, Outcome, Planned, Skip,
};
use grain_id::GrainId;

struct Photos;
impl AppExtension for Photos {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
}

/// Rewrites `list.m3u8` after a batch as the music app does: each line naming a moved file
/// names its new place.
struct Playlist;
impl AppExtension for Playlist {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image)
    }
    fn wants_extra(&self, rel: &str) -> bool {
        rel.ends_with(".m3u8")
    }
    fn after_moves(&self, ctx: &ExtContext, moves: &[(String, String)]) -> Vec<Rewrite> {
        let Ok(before) = fs::read_to_string(ctx.library.root().join("list.m3u8")) else {
            return Vec::new();
        };
        let after: String = before
            .lines()
            .map(|l| {
                let moved = moves.iter().find(|(from, _)| from == l);
                format!("{}\n", moved.map_or(l, |(_, to)| to.as_str()))
            })
            .collect();
        vec![Rewrite {
            path: "list.m3u8".into(),
            before,
            after,
        }]
    }
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    lib: Arc<Library>,
    cache: Arc<Cache>,
    journal: ServerJournal,
    /// The journal's clock, in milliseconds since the epoch.
    now: Arc<AtomicI64>,
}

fn env() -> Env {
    env_ext(Arc::new(Photos))
}

/// `a.jpg` with a sidecar and `b.jpg` at the root, scanned.
fn env_ext(ext: Arc<dyn AppExtension>) -> Env {
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
    fs::write(root.join("b.jpg"), "photo b").unwrap();
    lib.update_sidecar(Path::new("a.jpg"), |sc| {
        sc.subjects.insert("x".into());
    })
    .unwrap();
    scan(&lib, &cache, ext.as_ref()).unwrap();
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let now = Arc::new(AtomicI64::new(start));
    let clock = now.clone();
    let journal = open_journal(
        JournalCtx {
            library: lib.clone(),
            cache: cache.clone(),
            ext,
        },
        Some(Arc::new(move || clock.load(Ordering::SeqCst))),
    )
    .unwrap();
    Env {
        _dir: dir,
        root,
        lib,
        cache,
        journal,
        now,
    }
}

impl Env {
    fn owner(&self) -> Actor {
        Actor::person(self.lib.owner().to_string())
    }

    fn expect(&self, rel: &str) -> Expect {
        let rec = self.cache.get_by_path(rel).unwrap().unwrap();
        Expect {
            size: rec.size,
            mtime_ns: rec.mtime_ns,
            file_hash: None,
            hash: rec.hash,
        }
    }

    fn mv(&self, from: &str, to: &str) -> Planned {
        Planned::new::<Move>(&MovePlan {
            from: from.into(),
            to: to.into(),
            expect: self.expect(from),
        })
    }

    fn trash(&self, from: &str) -> Planned {
        Planned::new::<Trash>(&TrashPlan::Trash {
            from: from.into(),
            trash_id: GrainId::random(),
            expect: self.expect(from),
        })
    }

    fn tidy(&self, moves: &[(&str, Option<&str>)]) -> Planned {
        Planned::new::<OrganizeTidy>(&TidyPlan {
            user: self.lib.owner(),
            moves: moves
                .iter()
                .map(|(from, to)| TidyMove {
                    from: from.to_string(),
                    to: to.map(str::to_string),
                })
                .collect(),
        })
    }

    fn run_as(&self, ops: Vec<Planned>, actor: &Actor) -> BatchRecords {
        let id = self
            .journal
            .submit(ops, &Origin::new("test"), actor, None)
            .unwrap();
        self.journal.wait(id).unwrap()
    }

    fn run(&self, ops: Vec<Planned>) -> BatchRecords {
        self.run_as(ops, &self.owner())
    }

    fn undo_as(&self, id: BatchId, actor: &Actor) -> Result<BatchRecords, JournalError> {
        let u = self.journal.undo_batch(id, &Origin::new("test"), actor)?;
        self.journal.wait(u)
    }

    fn undo(&self, id: BatchId) -> BatchRecords {
        self.undo_as(id, &self.owner()).unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn cached(&self, rel: &str) -> bool {
        self.cache.get_by_path(rel).unwrap().is_some()
    }
}

fn of_kind<'a>(b: &'a BatchRecords, kind: &str) -> Vec<&'a OpRecord> {
    b.ops.iter().filter(|o| o.kind == kind).collect()
}

fn done_move(r: &OpRecord) -> MoveDone {
    match &r.outcome {
        Outcome::Done(v) => serde_json::from_value(v.clone()).unwrap(),
        other => panic!("not done: {other:?}"),
    }
}

fn skip(r: &OpRecord) -> &Skip {
    match &r.outcome {
        Outcome::Skipped(s) => s,
        other => panic!("not skipped: {other:?}"),
    }
}

#[test]
fn move_then_undo_restores_file_sidecar_and_cache() {
    let e = env();
    let id = e.cache.get_by_path("a.jpg").unwrap().unwrap().id;
    let b = e.run(vec![e.mv("a.jpg", "2025/c.jpg")]);
    assert!(b.summary.complete && !b.summary.interrupted);
    let d = done_move(of_kind(&b, "move")[0]);
    assert_eq!((d.from.as_str(), d.to.as_str()), ("a.jpg", "2025/c.jpg"));
    let new_id = item_id_for(&e.lib.config().id, "2025/c.jpg");
    assert_eq!(d.item_id, Some(new_id.0.clone()));
    assert!(!e.path("a.jpg").exists() && !e.path("a.jpg.xmp").exists());
    assert!(e.path("2025/c.jpg").exists() && e.path("2025/c.jpg.xmp").exists());
    assert!(e.cached("2025/c.jpg") && !e.cached("a.jpg"));

    let u = e.undo(b.summary.begin.batch_id);
    assert_eq!(u.summary.begin.undo_of, Some(b.summary.begin.batch_id));
    let back = done_move(of_kind(&u, "move")[0]);
    assert_eq!(
        (back.from.as_str(), back.to.as_str()),
        ("2025/c.jpg", "a.jpg")
    );
    assert_eq!(fs::read(e.path("a.jpg")).unwrap(), b"photo a");
    assert!(e.path("a.jpg.xmp").exists());
    assert!(!e.path("2025/c.jpg").exists() && !e.path("2025/c.jpg.xmp").exists());
    assert_eq!(e.cache.get_by_path("a.jpg").unwrap().unwrap().id, id);
    assert!(!e.cached("2025/c.jpg"));
    let summary = e.journal.status(b.summary.begin.batch_id).unwrap().summary;
    assert_eq!(summary.undone_by, Some(u.summary.begin.batch_id));
}

#[test]
fn trash_then_undo_restores_the_path() {
    let e = env();
    hash_pending(&e.lib, &e.cache, &Default::default()).unwrap();
    let hashed = e.cache.get_by_path("a.jpg").unwrap().unwrap().hash;
    assert!(hashed.is_some());
    let b = e.run(vec![e.trash("a.jpg")]);
    let d = done_move(of_kind(&b, "trash")[0]);
    assert_eq!(d.from, "a.jpg");
    assert!(d.to.starts_with(".asteria/trash/"), "{}", d.to);
    assert!(d.to.ends_with("/a.jpg"), "{}", d.to);
    assert_eq!(d.item_id, None);
    assert!(e.path(&d.to).exists() && e.path(&format!("{}.xmp", d.to)).exists());
    assert!(!e.path("a.jpg").exists() && !e.cached("a.jpg"));

    let u = e.undo(b.summary.begin.batch_id);
    let back = done_move(of_kind(&u, "trash")[0]);
    assert_eq!(
        (back.from.as_str(), back.to.as_str()),
        (d.to.as_str(), "a.jpg")
    );
    assert_eq!(fs::read(e.path("a.jpg")).unwrap(), b"photo a");
    assert!(e.path("a.jpg.xmp").exists() && !e.path(&d.to).exists());
    // The hash comes back with the item: the trash kept it for the undo.
    assert_eq!(e.cache.get_by_path("a.jpg").unwrap().unwrap().hash, hashed);
}

#[test]
fn undo_skips_a_file_moved_since() {
    let e = env();
    let first = e.run(vec![e.mv("a.jpg", "b2.jpg")]);
    e.run(vec![e.mv("b2.jpg", "c.jpg")]);
    let u = e.undo(first.summary.begin.batch_id);
    // The file is not where the first batch left it.
    assert_eq!(skip(of_kind(&u, "move")[0]), &Skip::NotFound);
    assert_eq!(fs::read(e.path("c.jpg")).unwrap(), b"photo a");
    assert!(e.path("c.jpg.xmp").exists());
    assert!(!e.path("a.jpg").exists() && !e.path("b2.jpg").exists());
}

#[test]
fn playlist_rewrite_is_undone_unless_changed() {
    let e = env_ext(Arc::new(Playlist));
    fs::write(e.path("list.m3u8"), "a.jpg\nb.jpg\n").unwrap();
    let b = e.run(vec![
        e.mv("a.jpg", "c.jpg"),
        e.tidy(&[("a.jpg", Some("c.jpg"))]),
    ]);
    assert_eq!(
        fs::read_to_string(e.path("list.m3u8")).unwrap(),
        "c.jpg\nb.jpg\n"
    );
    let tidy = of_kind(&b, "organize_tidy")[0];
    let rw = of_kind(&b, "rewrite_playlist")[0];
    assert_eq!(rw.parent, Some(tidy.op_id));
    assert!(matches!(rw.outcome, Outcome::Done(_)));
    e.undo(b.summary.begin.batch_id);
    assert_eq!(
        fs::read_to_string(e.path("list.m3u8")).unwrap(),
        "a.jpg\nb.jpg\n"
    );
    assert!(e.path("a.jpg").exists());

    // Edited after the batch: the undo leaves the edit, and still moves the file back.
    let b = e.run(vec![
        e.mv("a.jpg", "c.jpg"),
        e.tidy(&[("a.jpg", Some("c.jpg"))]),
    ]);
    fs::write(e.path("list.m3u8"), "edited\n").unwrap();
    let u = e.undo(b.summary.begin.batch_id);
    let rw = of_kind(&u, "rewrite_playlist")[0];
    assert_eq!(skip(rw), &Skip::PreconditionFailed);
    assert_eq!(fs::read_to_string(e.path("list.m3u8")).unwrap(), "edited\n");
    assert!(e.path("a.jpg").exists() && !e.path("c.jpg").exists());
}

#[test]
fn a_tidy_removes_emptied_folders_and_tells_the_app_only_real_moves() {
    let e = env_ext(Arc::new(Playlist));
    fs::write(e.path("list.m3u8"), "a.jpg\nb.jpg\n").unwrap();
    fs::create_dir(e.path("sub")).unwrap();
    fs::rename(e.path("b.jpg"), e.path("sub/b.jpg")).unwrap();
    scan(&e.lib, &e.cache, &Playlist).unwrap();
    fs::write(e.path("taken.jpg"), "someone else").unwrap();
    // The first move cannot go ahead: the tidy must not report it.
    let b = e.run(vec![
        e.mv("a.jpg", "taken.jpg"),
        e.mv("sub/b.jpg", "b.jpg"),
        e.tidy(&[("a.jpg", Some("taken.jpg")), ("sub/b.jpg", Some("b.jpg"))]),
    ]);
    assert_eq!(skip(of_kind(&b, "move")[0]), &Skip::Exists);
    assert!(!e.path("sub").exists());
    let rw = of_kind(&b, "rewrite_playlist")[0];
    let Outcome::Done(v) = &rw.outcome else {
        panic!("{rw:?}")
    };
    assert_eq!(v["after"], "a.jpg\nb.jpg\n");
}

#[test]
fn undo_is_for_the_same_user() {
    let e = env();
    let agent = Actor::agent(e.lib.owner().to_string());
    let b = e.run_as(vec![e.mv("a.jpg", "c.jpg")], &agent);
    let other = Actor::person(GrainId::random().to_string());
    let err = e
        .undo_as(b.summary.begin.batch_id, &other)
        .expect_err("another user may not undo it");
    assert_eq!(rpc_error(err).code, codes::FORBIDDEN);
    assert!(e.path("c.jpg").exists());
    e.undo_as(b.summary.begin.batch_id, &e.owner()).unwrap();
    assert!(e.path("a.jpg").exists());
}

#[test]
fn an_old_batch_cannot_be_undone() {
    let e = env();
    let b = e.run(vec![e.mv("a.jpg", "c.jpg")]);
    e.now.fetch_add(31 * DAY_MS, Ordering::SeqCst);
    let id = b.summary.begin.batch_id;
    let err = e.journal.check_undo(id, &e.owner()).unwrap_err();
    assert_eq!(rpc_error(err).code, codes::UNDO_EXPIRED);
    let err = e.undo_as(id, &e.owner()).unwrap_err();
    assert_eq!(rpc_error(err).code, codes::UNDO_EXPIRED);
    assert!(e.path("c.jpg").exists());
}

#[test]
fn redo_trashes_again() {
    let e = env();
    let b = e.run(vec![e.trash("a.jpg")]);
    let trashed = done_move(of_kind(&b, "trash")[0]).to;
    let u = e.undo(b.summary.begin.batch_id);
    assert!(e.path("a.jpg").exists());
    let r = e.undo(u.summary.begin.batch_id);
    assert_eq!(r.summary.begin.redo_of, Some(u.summary.begin.batch_id));
    assert!(!e.path("a.jpg").exists() && e.path(&trashed).exists());
    assert!(!e.cached("a.jpg"));
}
