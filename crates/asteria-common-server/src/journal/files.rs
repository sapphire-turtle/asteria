//! The file operations of `organize.apply`: `move`, `trash`, the `organize_tidy` that closes
//! every batch, and the `rewrite_playlist` it leads to. Each one changes the original files
//! and the cache; the Journal records what came of it.

use std::path::Path;

use asteria_common_core::UserId;
use asteria_common_core::fsutil::atomic_write;
use corundum_framework::journal::{Applied, OpKind, Operation, Planned, Registry, Skip};
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use super::JournalCtx;
use super::map::skip_of;
use crate::cache::{ExtraRecord, StoredHash};
use crate::ext::{ExtContext, Rewrite};
use crate::organize::{self, OpCtx, PlannedOp, Sim, Target};
use crate::scan::mtime_ns;

/// What the file looked like when the operation was planned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expect {
    pub size: u64,
    pub mtime_ns: i128,
    /// Whole-file hash (`file/1`); compared when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_hash: Option<String>,
    /// Brought back into the cache when a file is restored from the trash; never compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<StoredHash>,
}

/// Move `from` to `to` (library-relative), provided the file is still what was planned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MovePlan {
    pub from: String,
    pub to: String,
    pub expect: Expect,
}

/// A file that moved, as its undo needs it: the move back expects the same stamp, since a
/// rename keeps size and time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoveDone {
    pub from: String,
    pub to: String,
    /// The item's new id; `None` where the library shows no item (the trash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub expect: Expect,
}

/// A trash, or, as its undo or redo, a move into or out of the trash. corundum applies an
/// undo under the kind of the operation it reverses, so the trash's plan can be a move too.
/// The two tell apart by `trash_id` and `to`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TrashPlan {
    /// To `trash_rel(trash_id, from)`; `trash_id` is chosen when planning.
    Trash {
        from: String,
        trash_id: GrainId,
        expect: Expect,
    },
    Move(MovePlan),
}

// By hand: serde's untagged enums cannot hold the `i128` of `Expect` while they try each form.
impl<'de> Deserialize<'de> for TrashPlan {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Either {
            from: String,
            #[serde(default)]
            trash_id: Option<GrainId>,
            #[serde(default)]
            to: Option<String>,
            expect: Expect,
        }
        let p = Either::deserialize(d)?;
        match (p.trash_id, p.to) {
            (Some(trash_id), None) => Ok(TrashPlan::Trash {
                from: p.from,
                trash_id,
                expect: p.expect,
            }),
            (None, Some(to)) => Ok(TrashPlan::Move(MovePlan {
                from: p.from,
                to,
                expect: p.expect,
            })),
            _ => Err(serde::de::Error::custom(
                "a trash plan has either trash_id or to",
            )),
        }
    }
}

/// One move of the batch an `organize_tidy` closes; `to` is `None` for a trash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TidyMove {
    pub from: String,
    pub to: Option<String>,
}

/// The moves of the batch, and the user the app's `after_moves` acts for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TidyPlan {
    pub user: UserId,
    pub moves: Vec<TidyMove>,
}

pub struct Move;
pub struct Trash;
pub struct OrganizeTidy;
pub struct RewritePlaylist;

impl OpKind for Move {
    const KIND: &'static str = "move";
    type Plan = MovePlan;
    type Done = MoveDone;
}

impl OpKind for Trash {
    const KIND: &'static str = "trash";
    type Plan = TrashPlan;
    type Done = MoveDone;
}

impl OpKind for OrganizeTidy {
    const KIND: &'static str = "organize_tidy";
    type Plan = TidyPlan;
    /// The plan, with only the moves that happened.
    type Done = TidyPlan;
}

impl OpKind for RewritePlaylist {
    const KIND: &'static str = "rewrite_playlist";
    type Plan = Rewrite;
    type Done = Rewrite;
}

/// Registers the file operations.
pub(crate) fn register(reg: &mut Registry<JournalCtx>) {
    reg.register(Move)
        .register(Trash)
        .register(OrganizeTidy)
        .register(RewritePlaylist);
}

fn op_ctx(ctx: &JournalCtx) -> OpCtx<'_> {
    OpCtx {
        library: &ctx.library,
        cache: &ctx.cache,
        ext: ctx.ext.as_ref(),
    }
}

impl MovePlan {
    pub(crate) fn op(&self) -> PlannedOp {
        PlannedOp {
            from: self.from.clone(),
            to: Target::Path(self.to.clone()),
            expect: self.expect.clone(),
        }
    }
}

impl TrashPlan {
    pub(crate) fn op(&self) -> PlannedOp {
        match self {
            TrashPlan::Trash {
                from,
                trash_id,
                expect,
            } => PlannedOp {
                from: from.clone(),
                to: Target::Trash(*trash_id),
                expect: expect.clone(),
            },
            TrashPlan::Move(m) => m.op(),
        }
    }
}

impl MoveDone {
    /// The move that puts the file back.
    pub(crate) fn back(&self) -> MovePlan {
        MovePlan {
            from: self.to.clone(),
            to: self.from.clone(),
            expect: self.expect.clone(),
        }
    }
}

/// The planned operation `kind` (`move` or `trash`) records, if its plan reads as one.
pub(crate) fn planned_op(kind: &str, plan: &serde_json::Value) -> Option<PlannedOp> {
    match kind {
        Move::KIND => serde_json::from_value::<MovePlan>(plan.clone())
            .ok()
            .map(|p| p.op()),
        Trash::KIND => serde_json::from_value::<TrashPlan>(plan.clone())
            .ok()
            .map(|p| p.op()),
        _ => None,
    }
}

/// The plan of `op` for the Journal.
pub(crate) fn planned(op: &PlannedOp) -> Planned {
    match &op.to {
        Target::Path(to) => Planned::new::<Move>(&MovePlan {
            from: op.from.clone(),
            to: to.clone(),
            expect: op.expect.clone(),
        }),
        Target::Trash(trash_id) => Planned::new::<Trash>(&TrashPlan::Trash {
            from: op.from.clone(),
            trash_id: *trash_id,
            expect: op.expect.clone(),
        }),
    }
}

fn check(ctx: &JournalCtx, op: &PlannedOp) -> Result<(), Skip> {
    organize::check(&op_ctx(ctx), op, &Sim::default()).map_err(skip_of)
}

fn apply(ctx: &JournalCtx, op: &PlannedOp) -> Result<Applied<MoveDone>, Skip> {
    let d = organize::apply(&op_ctx(ctx), op).map_err(skip_of)?;
    Ok(Applied::new(MoveDone {
        from: op.from.clone(),
        to: d.to,
        item_id: d.record.map(|r| r.id.0),
        expect: Expect {
            size: op.expect.size,
            mtime_ns: op.expect.mtime_ns,
            file_hash: None,
            hash: d.hash,
        },
    }))
}

impl Operation<JournalCtx> for Move {
    fn check(&self, ctx: &JournalCtx, plan: &MovePlan) -> Result<(), Skip> {
        check(ctx, &plan.op())
    }

    fn apply(&self, ctx: &JournalCtx, plan: &MovePlan) -> Result<Applied<MoveDone>, Skip> {
        apply(ctx, &plan.op())
    }

    fn inverse(&self, done: &MoveDone) -> Option<MovePlan> {
        Some(done.back())
    }
}

impl Operation<JournalCtx> for Trash {
    fn check(&self, ctx: &JournalCtx, plan: &TrashPlan) -> Result<(), Skip> {
        check(ctx, &plan.op())
    }

    fn apply(&self, ctx: &JournalCtx, plan: &TrashPlan) -> Result<Applied<MoveDone>, Skip> {
        apply(ctx, &plan.op())
    }

    fn inverse(&self, done: &MoveDone) -> Option<TrashPlan> {
        Some(TrashPlan::Move(done.back()))
    }
}

/// Whether the move happened: its source is gone and, for a move, its target is there (a
/// case-only rename on a case-insensitive filesystem leaves one file under both names).
fn moved(root: &Path, m: &TidyMove) -> bool {
    let from = root.join(&m.from);
    let gone = !organize::exists(&from).unwrap_or(true);
    match &m.to {
        None => gone,
        Some(to) => {
            let to = root.join(to);
            organize::exists(&to).unwrap_or(false)
                && (gone || same_file::is_same_file(&from, &to).unwrap_or(false))
        }
    }
}

impl Operation<JournalCtx> for OrganizeTidy {
    /// Removes the folders the batch's moves emptied, and runs the app's rewrites for them
    /// as its children.
    fn apply(&self, ctx: &JournalCtx, plan: &TidyPlan) -> Result<Applied<TidyPlan>, Skip> {
        let root = ctx.library.root();
        let kept: Vec<TidyMove> = plan
            .moves
            .iter()
            .filter(|m| moved(root, m))
            .cloned()
            .collect();
        let sources: Vec<String> = kept.iter().map(|m| m.from.clone()).collect();
        organize::remove_emptied_folders(root, &sources);
        let pairs: Vec<(String, String)> = kept
            .iter()
            .filter_map(|m| Some((m.from.clone(), m.to.clone()?)))
            .collect();
        let ectx = ExtContext {
            library: ctx.library.clone(),
            cache: ctx.cache.clone(),
            user: plan.user,
            owner: ctx.library.owner(),
        };
        let children = organize::rewrites_after(ctx.ext.as_ref(), &ectx, &pairs)
            .iter()
            .map(Planned::new::<RewritePlaylist>)
            .collect();
        Ok(Applied {
            done: TidyPlan {
                user: plan.user,
                moves: kept,
            },
            children,
        })
    }

    /// A tidy that changes nothing: the moves are undone by their own inverses, and the
    /// rewrites by theirs.
    fn inverse(&self, done: &TidyPlan) -> Option<TidyPlan> {
        Some(TidyPlan {
            user: done.user,
            moves: Vec::new(),
        })
    }
}

impl Operation<JournalCtx> for RewritePlaylist {
    /// Writes `after` over `path` if the file still holds `before`, and updates its extras
    /// record.
    fn apply(&self, ctx: &JournalCtx, rw: &Rewrite) -> Result<Applied<Rewrite>, Skip> {
        if !organize::safe_rel(&rw.path) {
            tracing::warn!(path = %rw.path, "rewrite refused: not a library path");
            return Err(Skip::InvalidPath);
        }
        let abs = ctx.library.root().join(&rw.path);
        // Writing over a link would replace it with a plain file.
        if std::fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
            tracing::info!(path = %rw.path, "rewrite skipped: the playlist is a symlink");
            return Err(Skip::InvalidPath);
        }
        match std::fs::read_to_string(&abs) {
            Ok(now) if now == rw.before => {}
            Ok(_) => {
                tracing::info!(path = %rw.path, "rewrite skipped: the file changed meanwhile");
                return Err(Skip::PreconditionFailed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Skip::NotFound),
            Err(e) => {
                tracing::warn!(path = %rw.path, error = %e, "rewrite skipped: cannot read the file");
                return Err(Skip::Io(e.to_string()));
            }
        }
        atomic_write(&abs, rw.after.as_bytes()).map_err(|e| {
            tracing::warn!(path = %rw.path, error = %e, "rewrite failed");
            Skip::Io(e.to_string())
        })?;
        if ctx.ext.wants_extra(&rw.path)
            && let Ok(meta) = std::fs::metadata(&abs)
        {
            let rec = ExtraRecord {
                size: meta.len(),
                mtime_ns: mtime_ns(&meta),
            };
            if let Err(e) = ctx.cache.put_extras(&[(rw.path.clone(), rec)]) {
                tracing::warn!(path = %rw.path, error = %e, "extras not updated; the next scan will fix it");
            }
        }
        Ok(Applied::new(rw.clone()))
    }

    fn inverse(&self, done: &Rewrite) -> Option<Rewrite> {
        Some(Rewrite {
            path: done.path.clone(),
            before: done.after.clone(),
            after: done.before.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use asteria_common_core::{Library, MediaKind};
    use corundum_framework::journal::{Actor, Origin};

    use super::*;
    use crate::cache::Cache;
    use crate::ext::AppExtension;
    use crate::journal::{ServerJournal, open_journal};
    use crate::organize::hold;
    use crate::scan::scan;

    struct All;
    impl AppExtension for All {
        fn accepts(&self, _: MediaKind) -> bool {
            true
        }
    }

    fn journal_over(lib: &Arc<Library>, cache: &Arc<Cache>) -> ServerJournal {
        let ctx = JournalCtx {
            library: lib.clone(),
            cache: cache.clone(),
            ext: Arc::new(All),
        };
        open_journal(ctx, None).unwrap()
    }

    fn mv(cache: &Cache, from: &str, to: &str) -> Planned {
        let rec = cache.get_by_path(from).unwrap().unwrap();
        Planned::new::<Move>(&MovePlan {
            from: from.into(),
            to: to.into(),
            expect: Expect {
                size: rec.size,
                mtime_ns: rec.mtime_ns,
                file_hash: None,
                hash: None,
            },
        })
    }

    /// A batch that stops after its second of three moves, as a crash would leave it, is
    /// listed as not complete once the store is opened again, and its done moves undo.
    #[test]
    fn an_interrupted_batch_is_listed_and_undoable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        for name in ["a.jpg", "b.jpg", "c.jpg"] {
            fs::write(root.join(name), name).unwrap();
        }
        let lib = Arc::new(
            Library::init(&root, "o")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        scan(&lib, &cache, &All).unwrap();
        let journal = journal_over(&lib, &cache);
        let owner = Actor::person(lib.owner().to_string());
        let origin = Origin::new("test");

        // The third move stops before it touches anything, and stays there.
        let held = hold::at(root.join("c.jpg"));
        let id = journal
            .submit(
                vec![
                    mv(&cache, "a.jpg", "x/a.jpg"),
                    mv(&cache, "b.jpg", "x/b.jpg"),
                    mv(&cache, "c.jpg", "x/c.jpg"),
                ],
                &origin,
                &owner,
                Some("by folder".into()),
            )
            .unwrap();
        held.entered
            .recv_timeout(Duration::from_secs(20))
            .expect("the third move starts");

        // The crash: the journal's files as they are now, opened again by a new store (its
        // lock file says the directory is not of the old format).
        let state2 = dir.path().join("state2");
        fs::create_dir_all(state2.join("journal")).unwrap();
        fs::write(state2.join("journal/journal.lock"), "").unwrap();
        for f in fs::read_dir(dir.path().join("state/journal")).unwrap() {
            let f = f.unwrap();
            if f.file_name().to_string_lossy().ends_with(".jsonl") {
                fs::copy(f.path(), state2.join("journal").join(f.file_name())).unwrap();
            }
        }
        let lib2 = Arc::new(Library::open(&root).unwrap().with_state_dir(&state2));
        let reopened = journal_over(&lib2, &cache);

        let h = organize::history(&reopened, 10, None, None).unwrap();
        let b = &h.batches[0];
        assert_eq!(b.batch_id, id.to_string());
        assert!(!b.complete, "{b:?}");
        assert_eq!(b.done, 2);
        assert_eq!(b.label.as_deref(), Some("by folder"));
        let s = organize::status(&reopened, id).unwrap();
        assert!(!s.running && !s.complete, "{s:?}");
        assert_eq!(s.done, 2);

        let u = reopened.undo_batch(id, &origin, &owner).unwrap();
        let undone = reopened.wait(u).unwrap();
        assert_eq!(undone.summary.done, 2, "{undone:?}");
        assert!(root.join("a.jpg").exists() && root.join("b.jpg").exists());
        assert!(!root.join("x/a.jpg").exists() && !root.join("x/b.jpg").exists());
        assert!(root.join("c.jpg").exists());

        // Let the first journal's worker go: the held move gives up.
        held.release.send(()).unwrap();
        let rest = journal.wait(id).unwrap();
        assert_eq!(rest.summary.skipped, 1);
        assert!(root.join("c.jpg").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_playlist_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(Library::init(&root, "o").unwrap());
        let cache = Arc::new(Cache::open(&dir.path().join("c"), &lib.config().id).unwrap());
        fs::write(root.join("real.m3u"), "a.jpg\n").unwrap();
        std::os::unix::fs::symlink("real.m3u", root.join("list.m3u")).unwrap();
        let ctx = JournalCtx {
            library: lib,
            cache,
            ext: Arc::new(All),
        };
        let rw = Rewrite {
            path: "list.m3u".into(),
            before: "a.jpg\n".into(),
            after: "c.jpg\n".into(),
        };
        let r = RewritePlaylist.apply(&ctx, &rw);
        assert_eq!(r.unwrap_err(), Skip::InvalidPath);
        let link = fs::symlink_metadata(root.join("list.m3u")).unwrap();
        assert!(link.file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(root.join("real.m3u")).unwrap(),
            "a.jpg\n"
        );
    }

    #[test]
    fn a_trash_plan_reads_as_a_trash_or_a_move() {
        let expect = Expect {
            size: 1,
            mtime_ns: 2,
            file_hash: None,
            hash: None,
        };
        let trash = TrashPlan::Trash {
            from: "a.jpg".into(),
            trash_id: GrainId::random(),
            expect: expect.clone(),
        };
        let back = TrashPlan::Move(MovePlan {
            from: ".asteria/trash/x/a.jpg".into(),
            to: "a.jpg".into(),
            expect,
        });
        for plan in [trash, back] {
            let v = serde_json::to_value(&plan).unwrap();
            assert_eq!(serde_json::from_value::<TrashPlan>(v).unwrap(), plan);
        }
        let both = serde_json::json!({
            "from": "a.jpg", "to": "b.jpg", "trash_id": GrainId::random(),
            "expect": {"size": 1, "mtime_ns": 2},
        });
        assert!(serde_json::from_value::<TrashPlan>(both).is_err());
    }
}
