//! The file operations of `organize.apply`: `move`, `trash`, the `organize_tidy` that closes
//! every batch, and the `rewrite_playlist` it leads to; the `upload` that puts a committed
//! upload in place; and the `relink_sidecar` that moves an orphaned sidecar to its item. Each
//! one changes the original files (and, but for `upload`, the cache); the Journal records what
//! came of it.

use std::io::ErrorKind;
use std::path::Path;

use asteria_common_core::fsutil::atomic_write;
use asteria_common_core::hash::file_hash;
use asteria_common_core::paths::{is_sidecar, sidecar_path_for};
use asteria_common_core::{UserId, classify};
use corundum_framework::journal::{Applied, OpKind, Operation, Planned, Registry, Skip};
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use super::JournalCtx;
use super::map::skip_of;
use crate::cache::{ExtraRecord, StoredHash, item_id_for};
use crate::ext::{ExtContext, Rewrite};
use crate::fsutil::{DirError, check_dirs_inside, move_no_clobber};
use crate::hashing;
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
        TrashParts::deserialize(d)?.plan()
    }
}

/// The fields of a trash plan, of either form, as read.
#[derive(Deserialize)]
struct TrashParts {
    from: String,
    #[serde(default)]
    trash_id: Option<GrainId>,
    #[serde(default)]
    to: Option<String>,
    expect: Expect,
}

impl TrashParts {
    fn plan<E: serde::de::Error>(self) -> Result<TrashPlan, E> {
        match (self.trash_id, self.to) {
            (Some(trash_id), None) => Ok(TrashPlan::Trash {
                from: self.from,
                trash_id,
                expect: self.expect,
            }),
            (None, Some(to)) => Ok(TrashPlan::Move(MovePlan {
                from: self.from,
                to,
                expect: self.expect,
            })),
            _ => Err(E::custom("a trash plan has either trash_id or to")),
        }
    }
}

/// A file the commit's first phase staged (library-relative), where it goes, its upload
/// session, and its whole-file hash (`file/1`), known from the upload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagedFile {
    pub staged: String,
    pub to: String,
    pub session: String,
    pub file_hash: String,
}

/// An upload put in place, or, as its undo or redo, a trash form: corundum applies an undo
/// under the kind of the operation it reverses. The two tell apart by `staged`.
///
/// A media file and the sidecar uploaded with it are one operation: they go into place,
/// into the trash and back together, or not at all.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum UploadPlan {
    /// Rename the staged file into place, never over an existing file, then its sidecar
    /// beside it.
    Place {
        #[serde(flatten)]
        file: StagedFile,
        #[serde(skip_serializing_if = "Option::is_none")]
        sidecar: Option<StagedFile>,
    },
    /// A trash plan, provided the file's sidecar, which the trash takes along, is still the
    /// one uploaded with it (`sidecar`), or there is none when none was (`None`).
    Trash {
        #[serde(flatten)]
        plan: TrashPlan,
        #[serde(skip_serializing_if = "Option::is_none")]
        sidecar: Option<Expect>,
    },
}

// By hand, as `TrashPlan`'s; `sidecar` is a staged file or a stamp, by the form.
impl<'de> Deserialize<'de> for UploadPlan {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(Deserialize)]
        struct Parts {
            staged: Option<String>,
            session: Option<String>,
            file_hash: Option<String>,
            to: Option<String>,
            from: Option<String>,
            trash_id: Option<GrainId>,
            expect: Option<Expect>,
            sidecar: Option<serde_json::Value>,
        }
        let p = Parts::deserialize(d)?;
        fn sidecar<T: serde::de::DeserializeOwned, E: Error>(
            v: Option<serde_json::Value>,
        ) -> Result<Option<T>, E> {
            v.map(serde_json::from_value).transpose().map_err(E::custom)
        }
        match (p.staged, p.session, p.file_hash) {
            (Some(staged), Some(session), Some(file_hash))
                if p.from.is_none() && p.trash_id.is_none() && p.expect.is_none() =>
            {
                Ok(UploadPlan::Place {
                    file: StagedFile {
                        staged,
                        to: p.to.ok_or_else(|| D::Error::missing_field("to"))?,
                        session,
                        file_hash,
                    },
                    sidecar: sidecar(p.sidecar)?,
                })
            }
            (None, None, None) => Ok(UploadPlan::Trash {
                plan: TrashParts {
                    from: p.from.ok_or_else(|| D::Error::missing_field("from"))?,
                    trash_id: p.trash_id,
                    to: p.to,
                    expect: p.expect.ok_or_else(|| D::Error::missing_field("expect"))?,
                }
                .plan()?,
                sidecar: sidecar(p.sidecar)?,
            }),
            _ => Err(D::Error::custom(
                "an upload plan has staged, session and file_hash, or a trash plan's fields",
            )),
        }
    }
}

/// An upload in place, or a move into or out of the trash (`from` given).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadDone {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    /// The item's id; `None` for a file the app does not show, or the trash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The file as it was left: what its undo expects.
    pub expect: Expect,
    /// Its sidecar as it was left, when one was uploaded with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<Expect>,
}

/// Move the sidecar `from` to `to` (library-relative): an orphan to the item whose content
/// it describes, or, as the undo, back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelinkPlan {
    pub from: String,
    pub to: String,
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
pub struct Upload;
pub struct RelinkSidecar;

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

impl OpKind for Upload {
    const KIND: &'static str = "upload";
    type Plan = UploadPlan;
    type Done = UploadDone;
}

impl OpKind for RelinkSidecar {
    const KIND: &'static str = "relink_sidecar";
    type Plan = RelinkPlan;
    type Done = RelinkPlan;
}

/// Registers the file operations.
pub(crate) fn register(reg: &mut Registry<JournalCtx>) {
    reg.register(Move)
        .register(Trash)
        .register(OrganizeTidy)
        .register(RewritePlaylist)
        .register(Upload)
        .register(RelinkSidecar);
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

/// Whether the staged file can go to `to`: both plain library paths, the staged file there,
/// `to` free, and no link on the way (#32).
fn check_place(ctx: &JournalCtx, f: &StagedFile) -> Result<(), Skip> {
    let (staged, to) = (f.staged.as_str(), f.to.as_str());
    if !organize::safe_rel(staged) || !organize::safe_rel(to) {
        return Err(Skip::InvalidPath);
    }
    let root = ctx.library.root();
    let folder = to.rsplit_once('/').map_or("", |(dir, _)| dir);
    check_dirs_inside(root, folder).map_err(|e| match e {
        DirError::Linked(dir) => {
            tracing::warn!(path = %to, folder = %dir, "the upload's folder is a link");
            Skip::InvalidPath
        }
        DirError::Io(e) => io_skip("cannot check the upload's folder", to, &e),
    })?;
    match organize::exists(&root.join(staged)) {
        Ok(true) => {}
        Ok(false) => return Err(Skip::NotFound),
        Err(e) => return Err(io_skip("cannot stat the staged upload", to, &e)),
    }
    match organize::exists(&root.join(to)) {
        Ok(false) => Ok(()),
        Ok(true) => Err(Skip::Exists),
        Err(e) => Err(io_skip("cannot stat the upload's target", to, &e)),
    }
}

/// A skip for an I/O error, logged with the library-relative `path`.
pub(crate) fn io_skip(what: &str, path: &str, e: &std::io::Error) -> Skip {
    tracing::warn!(%path, error = %e, "{what}");
    Skip::Io(e.to_string())
}

/// Renames the staged file into place, and gives how it was left.
fn place_one(ctx: &JournalCtx, f: &StagedFile) -> Result<Expect, Skip> {
    let root = ctx.library.root();
    let to_abs = root.join(&f.to);
    #[cfg(test)]
    crate::upload_commit::hold::before_place(&to_abs);
    check_place(ctx, f)?;
    move_no_clobber(&root.join(&f.staged), &to_abs).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => Skip::Exists,
        ErrorKind::NotFound => Skip::NotFound,
        _ => io_skip("cannot place the upload", &f.to, &e),
    })?;
    let meta =
        std::fs::metadata(&to_abs).map_err(|e| io_skip("cannot stat the upload", &f.to, &e))?;
    Ok(Expect {
        size: meta.len(),
        mtime_ns: mtime_ns(&meta),
        file_hash: Some(f.file_hash.clone()),
        hash: None,
    })
}

/// Puts the file in place, then its sidecar; if the sidecar cannot go, the file goes back to
/// where it was staged. The cache is left to the rescan the commit asks for.
fn place(
    ctx: &JournalCtx,
    file: &StagedFile,
    sidecar: Option<&StagedFile>,
) -> Result<Applied<UploadDone>, Skip> {
    let expect = place_one(ctx, file)?;
    let sidecar = match sidecar.map(|s| place_one(ctx, s)).transpose() {
        Ok(s) => s,
        Err(skip) => {
            let root = ctx.library.root();
            if let Err(e) = move_no_clobber(&root.join(&file.to), &root.join(&file.staged)) {
                tracing::error!(path = %file.to, error = %e,
                    "could not take an upload back after its sidecar failed to go in place");
            }
            return Err(skip);
        }
    };
    let to_abs = ctx.library.root().join(&file.to);
    let shown =
        !is_sidecar(Path::new(&file.to)) && classify(&to_abs).is_ok_and(|k| ctx.ext.accepts(k));
    Ok(Applied::new(UploadDone {
        from: None,
        to: file.to.clone(),
        item_id: shown.then(|| item_id_for(&ctx.library.config().id, &file.to).0),
        expect,
        sidecar,
    }))
}

/// Whether the sidecar of the library file `media` is as `want` says: the one uploaded with
/// it, unchanged, or none at all. A sidecar edited or made since (a rating) keeps the file
/// out of the trash, which would take it along.
fn check_upload_sidecar(ctx: &JournalCtx, media: &str, want: Option<&Expect>) -> Result<(), Skip> {
    let abs = sidecar_path_for(&ctx.library.root().join(media));
    let meta = match abs.symlink_metadata() {
        Ok(m) => Some(m),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(io_skip("cannot stat the upload's sidecar", media, &e)),
    };
    let same = match (want, meta) {
        (None, None) => true,
        (Some(want), Some(m)) => {
            m.is_file()
                && (m.len(), mtime_ns(&m)) == (want.size, want.mtime_ns)
                && match &want.file_hash {
                    None => true,
                    Some(h) => file_hash(&abs).is_ok_and(|have| &have.to_string() == h),
                }
        }
        _ => false,
    };
    if same {
        Ok(())
    } else {
        tracing::info!(path = %media, "the upload's sidecar changed since; leaving both");
        Err(Skip::PreconditionFailed)
    }
}

impl Operation<JournalCtx> for Upload {
    fn check(&self, ctx: &JournalCtx, plan: &UploadPlan) -> Result<(), Skip> {
        match plan {
            UploadPlan::Place { file, sidecar } => {
                check_place(ctx, file)?;
                sidecar.as_ref().map_or(Ok(()), |s| check_place(ctx, s))
            }
            UploadPlan::Trash { plan, sidecar } => {
                let op = plan.op();
                check_upload_sidecar(ctx, &op.from, sidecar.as_ref())?;
                check(ctx, &op)
            }
        }
    }

    fn apply(&self, ctx: &JournalCtx, plan: &UploadPlan) -> Result<Applied<UploadDone>, Skip> {
        match plan {
            UploadPlan::Place { file, sidecar } => place(ctx, file, sidecar.as_ref()),
            UploadPlan::Trash { plan, sidecar } => {
                let op = plan.op();
                check_upload_sidecar(ctx, &op.from, sidecar.as_ref())?;
                let d = apply(ctx, &op)?.done;
                Ok(Applied::new(UploadDone {
                    from: Some(d.from),
                    to: d.to,
                    item_id: d.item_id,
                    // The move keeps the content, so the hash still holds.
                    expect: Expect {
                        file_hash: op.expect.file_hash,
                        ..d.expect
                    },
                    sidecar: sidecar.clone(),
                }))
            }
        }
    }

    /// An upload in place goes to the trash, under an id of its own, if it and its sidecar
    /// are still the files that were placed; a move into or out of the trash goes back.
    fn inverse(&self, done: &UploadDone) -> Option<UploadPlan> {
        let plan = match &done.from {
            None => TrashPlan::Trash {
                from: done.to.clone(),
                trash_id: GrainId::random(),
                expect: done.expect.clone(),
            },
            Some(from) => TrashPlan::Move(
                MoveDone {
                    from: from.clone(),
                    to: done.to.clone(),
                    item_id: done.item_id.clone(),
                    expect: done.expect.clone(),
                }
                .back(),
            ),
        };
        Some(UploadPlan::Trash {
            plan,
            sidecar: done.sidecar.clone(),
        })
    }
}

impl Operation<JournalCtx> for RelinkSidecar {
    fn check(&self, ctx: &JournalCtx, plan: &RelinkPlan) -> Result<(), Skip> {
        hashing::check_sidecar_move(&ctx.library, &plan.from, &plan.to)
    }

    fn apply(&self, ctx: &JournalCtx, plan: &RelinkPlan) -> Result<Applied<RelinkPlan>, Skip> {
        hashing::move_sidecar(&ctx.library, &ctx.cache, &plan.from, &plan.to)?;
        Ok(Applied::new(plan.clone()))
    }

    fn inverse(&self, done: &RelinkPlan) -> Option<RelinkPlan> {
        Some(RelinkPlan {
            from: done.to.clone(),
            to: done.from.clone(),
        })
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

    #[test]
    fn an_upload_plan_reads_as_a_placement_or_a_trash_form() {
        let staged = |name: &str| StagedFile {
            staged: format!("Inbox/.{name}.tag.asteria-tmp"),
            to: format!("Inbox/{name}"),
            session: name.into(),
            file_hash: "blake3:00".into(),
        };
        let place = UploadPlan::Place {
            file: staged("a.jpg"),
            sidecar: None,
        };
        assert_eq!(
            serde_json::to_value(&place).unwrap(),
            serde_json::json!({
                "staged": "Inbox/.a.jpg.tag.asteria-tmp", "to": "Inbox/a.jpg",
                "session": "a.jpg", "file_hash": "blake3:00",
            })
        );
        let pair = UploadPlan::Place {
            file: staged("a.jpg"),
            sidecar: Some(staged("a.jpg.xmp")),
        };
        let expect = Expect {
            size: 1,
            mtime_ns: 2,
            file_hash: Some("blake3:00".into()),
            hash: None,
        };
        let trash = UploadPlan::Trash {
            plan: TrashPlan::Trash {
                from: "Inbox/a.jpg".into(),
                trash_id: GrainId::random(),
                expect: expect.clone(),
            },
            sidecar: Some(expect.clone()),
        };
        let back = UploadPlan::Trash {
            plan: TrashPlan::Move(MovePlan {
                from: ".asteria/trash/x/Inbox/a.jpg".into(),
                to: "Inbox/a.jpg".into(),
                expect,
            }),
            sidecar: None,
        };
        for plan in [place, pair, trash, back] {
            let v = serde_json::to_value(&plan).unwrap();
            assert_eq!(serde_json::from_value::<UploadPlan>(v).unwrap(), plan);
        }
        let mixed = serde_json::json!({
            "staged": "Inbox/.s", "to": "Inbox/a.jpg", "session": "s", "file_hash": "blake3:00",
            "expect": {"size": 1, "mtime_ns": 2},
        });
        assert!(serde_json::from_value::<UploadPlan>(mixed).is_err());
    }
}
