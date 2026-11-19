//! `upload.commit`: places a group of finished upload sessions under `Inbox/` (spec §4.7).
//!
//! Two phases, so that the heavy work never holds up the write queue:
//! - [`prepare`] seals the sessions, then checks and hashes them (and reads the sidecars)
//!   and stages each file in the group's folder: a hard link, or a synced copy when the
//!   sessions are on another filesystem than the library. All outside the queue;
//! - [`place`] runs in the write queue ([`crate::writer::Writer::commit_upload`]): it checks
//!   for duplicates again, numbers, has the Journal rename the staged files into place (one
//!   batch of `upload`s, see [`crate::journal::files`]) and leaves the tombstones.
//!
//! The seals are held from the start of `prepare` to the end of `place`. Staged files that
//! were not placed (a failure, or duplicates found) are removed. The caller asks for a
//! rescan afterwards; this module does not touch the cache.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use asteria_common_api::{PlacedFile, UploadCommitParams, UploadCommitResult, UploadWarning};
use asteria_common_core::fsutil::tmp_path_for;
use asteria_common_core::hash::{ContentHash, HashOutcome, HashScope, content_hash, file_hash};
use asteria_common_core::journal::Actor;
use asteria_common_core::paths::{media_path_for_sidecar, sidecar_path_for};
use asteria_common_core::upload_path::validate_inbox_path;
use asteria_common_core::{Error as FwError, Library, Sidecar, UserId};
use corundum_framework::journal::{
    Actor as JournalActor, BatchId, BatchRecords, JournalError, OpKind as _, Origin, Outcome,
    Planned,
};

use crate::cache::Cache;
use crate::error::ServerError;
use crate::fsutil::{DirError, create_dirs_inside, stage, sync_dir};
use crate::journal::files::{StagedFile, Upload, UploadPlan};
use crate::journal::map::actor_of;
use crate::journal::{POLL, ServerJournal, auto_label};
use crate::scan::rel_string;
use crate::upload::{Committed, SealGuard, UploadError, UploadStore};

/// Library-relative folder that every upload lands in.
pub const INBOX: &str = "Inbox";

/// Give up numbering after this many taken names.
const MAX_NUMBER: u32 = 10_000;

/// One file of the group, once its session is known to be complete.
struct Item {
    session: String,
    /// File name inside the group's folder, as the client asked.
    name: String,
    data: PathBuf,
    /// Payload hash; `None` for a sidecar.
    content: Option<HashOutcome>,
    /// Whole-file hash (`file/1`).
    file_hash: ContentHash,
    /// The file's content staged in the group's folder, under a temp name; renamed into
    /// place by [`place`].
    staged: PathBuf,
}

/// The group's folder, the folders made for it, the files staged in it and the sessions'
/// seals. Whatever is still staged when this is dropped is removed, and so are the folders
/// made for it, as far as they are empty, unless [`Staging::keep_folders`] was called; the
/// seals are released only after that.
struct Staging {
    dir: PathBuf,
    /// The topmost folder this commit made (`dir` and the ones between), if any.
    created_from: Option<PathBuf>,
    files: Vec<PathBuf>,
    _seals: Vec<SealGuard>,
}

impl Staging {
    /// Makes the library-relative folder `rel` below `root`, never through a link (#32).
    fn new(root: &Path, rel: &str, seals: Vec<SealGuard>) -> Result<Staging, UploadError> {
        let created_from = create_dirs_inside(root, rel).map_err(|e| match e {
            DirError::Linked(dir) => UploadError::Invalid(format!(
                "the folder {dir:?} is a link; uploads are not placed through links"
            )),
            DirError::Io(e) => e.into(),
        })?;
        Ok(Staging {
            dir: rel.split('/').fold(root.to_path_buf(), |d, s| d.join(s)),
            created_from,
            files: Vec::new(),
            _seals: seals,
        })
    }

    /// The group is in place: its folders stay.
    fn keep_folders(&mut self) {
        self.created_from = None;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        for f in &self.files {
            match fs::remove_file(f) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    tracing::warn!(path = %f.display(), error = %e, "could not remove a staged upload");
                }
                _ => {}
            }
        }
        if let Some(top) = &self.created_from {
            for d in self.dir.ancestors() {
                let _ = fs::remove_dir(d);
                if d == top {
                    break;
                }
            }
        }
    }
}

/// A group whose sessions are sealed, complete, hashed and staged, ready for [`place`].
pub struct Prepared {
    user: UserId,
    params: UploadCommitParams,
    /// The group's folder under `Inbox/` (`""` for `Inbox/` itself).
    folder: String,
    /// The files to place: the media and the sidecars that parse.
    kept: Vec<Item>,
    /// Sidecars that did not parse, and why.
    warnings: Vec<UploadWarning>,
    /// Their sessions.
    dropped: Vec<String>,
    /// The staged files; also holds the seals until the group is placed (or not).
    staging: Staging,
}

/// A group whose sessions are sealed, before the heavy part of [`prepare`].
pub struct Sealed {
    user: UserId,
    params: UploadCommitParams,
    folder: String,
    seals: Vec<SealGuard>,
}

/// What [`seal_group`] came to.
pub enum Begun {
    /// These sessions were committed before: this is that commit's answer.
    Answered(UploadCommitResult),
    Sealed(Sealed),
}

/// What [`prepare`] came to.
pub enum Prepare {
    /// These sessions were committed before: this is that commit's answer.
    Answered(UploadCommitResult),
    Ready(Prepared),
}

/// Places the group `params.files` under `Inbox/`: [`prepare`], then [`place`], journaled in
/// `journal` (which runs over `library` and `cache`) as `user` from `origin`. See spec §4.7
/// for the steps.
pub fn commit(
    library: &Library,
    cache: &Cache,
    journal: &ServerJournal,
    store: &Arc<UploadStore>,
    user: UserId,
    origin: &Origin,
    params: UploadCommitParams,
) -> Result<UploadCommitResult, UploadError> {
    match prepare(library, store, user, params)? {
        Prepare::Answered(result) => Ok(result),
        Prepare::Ready(p) => place(library, cache, journal, store, origin, p),
    }
}

/// The first phase of a commit, outside the write queue: [`seal_group`], then
/// [`prepare_sealed`]. Blocking.
pub fn prepare(
    library: &Library,
    store: &Arc<UploadStore>,
    user: UserId,
    params: UploadCommitParams,
) -> Result<Prepare, UploadError> {
    match seal_group(store, user, params)? {
        Begun::Answered(result) => Ok(Prepare::Answered(result)),
        Begun::Sealed(sealed) => prepare_sealed(library, store, sealed).map(Prepare::Ready),
    }
}

/// The quick start of a commit: checks the group, seals its sessions and answers a
/// repeated commit from the tombstones. Blocking.
pub fn seal_group(
    store: &Arc<UploadStore>,
    user: UserId,
    params: UploadCommitParams,
) -> Result<Begun, UploadError> {
    let folder = validate_group(&params)?.to_owned();

    // No chunk write or abort may touch a session while we hash and stage its file.
    let seals = params
        .files
        .iter()
        .map(|f| store.seal(user, &f.session))
        .collect::<Result<Vec<_>, _>>()?;

    if let Some(result) = earlier_commit(store, user, &params)? {
        return Ok(Begun::Answered(result));
    }
    Ok(Begun::Sealed(Sealed {
        user,
        params,
        folder,
        seals,
    }))
}

/// The heavy part of a commit's first phase: checks that every session is complete and
/// matches the hash claimed at `begin`, hashes the payloads, reads the sidecars and stages
/// the files in the group's folder. Blocking.
pub fn prepare_sealed(
    library: &Library,
    store: &Arc<UploadStore>,
    sealed: Sealed,
) -> Result<Prepared, UploadError> {
    let Sealed {
        user,
        params,
        folder,
        seals,
    } = sealed;
    let mut items = Vec::with_capacity(params.files.len());
    for f in &params.files {
        #[cfg(test)]
        hold::before_hash(&f.session);
        let done = store.completed(user, &f.session)?;
        let name = file_name(&f.path).to_string();
        let (content, whole) = if is_sidecar_name(&name) {
            (None, file_hash(&done.data_path).map_err(fw)?)
        } else {
            let o = content_hash(&done.data_path).map_err(fw)?;
            let whole = match o.scope {
                HashScope::File1 => o.hash,
                HashScope::MediaPayload1 => file_hash(&done.data_path).map_err(fw)?,
            };
            (Some(o), whole)
        };
        if whole != done.file_hash {
            store.remove(&f.session);
            return Err(UploadError::HashMismatch);
        }
        items.push((f.session.clone(), name, done.data_path, content, whole));
    }

    // A sidecar that does not parse is dropped; the media still goes in.
    let mut warnings = Vec::new();
    let mut dropped = Vec::new();
    let mut keep = Vec::with_capacity(items.len());
    for (session, name, data, content, whole) in items {
        if content.is_none() {
            match Sidecar::read(&data) {
                Ok(Some(_)) => {}
                Ok(None) => return Err(UploadError::NotFound),
                Err(FwError::MalformedSidecar { reason, .. }) => {
                    warnings.push(UploadWarning::SidecarDropped {
                        session: session.clone(),
                        reason,
                    });
                    dropped.push(session);
                    continue;
                }
                Err(e) => return Err(fw(e)),
            }
        }
        keep.push((session, name, data, content, whole));
    }

    // Staged on the library's filesystem now, so that placing them is only a rename.
    let rel = if folder.is_empty() {
        INBOX.to_string()
    } else {
        format!("{INBOX}/{folder}")
    };
    let mut staging = Staging::new(library.root(), &rel, seals)?;
    let mut kept = Vec::with_capacity(keep.len());
    for (session, name, data, content, file_hash) in keep {
        let staged = tmp_path_for(&staging.dir.join(&session));
        stage(&data, &staged)
            .map_err(|e| UploadError::Io(format!("could not stage {name}: {e}")))?;
        staging.files.push(staged.clone());
        kept.push(Item {
            session,
            name,
            data,
            content,
            file_hash,
            staged,
        });
    }
    Ok(Prepared {
        user,
        params,
        folder,
        kept,
        warnings,
        dropped,
        staging,
    })
}

/// The second phase of a commit, in the write queue: unless duplicates are allowed, looks
/// for the payloads in the library (again: it may have changed since the client's check),
/// then numbers the files, has `journal` rename them into place as one batch of `upload`s by
/// the committing user from `origin`, waits for it, and turns their sessions into tombstones.
/// A group the batch could not place whole is taken back out by undoing the batch. Blocking;
/// the seals are released when it returns.
pub fn place(
    library: &Library,
    cache: &Cache,
    journal: &ServerJournal,
    store: &UploadStore,
    origin: &Origin,
    prepared: Prepared,
) -> Result<UploadCommitResult, UploadError> {
    let Prepared {
        user,
        params,
        folder,
        kept,
        warnings,
        dropped,
        mut staging,
    } = prepared;
    if !params.allow_duplicate {
        let mut existing = BTreeSet::new();
        for o in kept.iter().filter_map(|i| i.content.as_ref()) {
            existing.extend(
                cache
                    .paths_with_hash(&o.hash.to_string(), &o.scope.to_string())
                    .map_err(server)?,
            );
        }
        if !existing.is_empty() {
            return Ok(UploadCommitResult::Duplicate {
                existing: existing.into_iter().collect(),
            });
        }
    }

    let dir = staging.dir.clone();
    let rel_dir = if folder.is_empty() {
        INBOX.to_string()
    } else {
        format!("{INBOX}/{folder}")
    };
    let n = free_number(&dir, &kept)?;
    let placed: Vec<(&Item, String)> = kept
        .iter()
        .map(|item| (item, format!("{rel_dir}/{}", target_name(&item.name, n))))
        .collect();
    let plans = upload_plans(library.root(), &placed)?;
    let actor = actor_of(Actor::User(user));
    let label = auto_label(Upload::KIND, placed.len());
    let id = journal
        .submit(plans, origin, &actor, label)
        .map_err(journal_error)?;
    // From here on the batch may have placed files: the commit answers only once it has
    // ended, and from what it did.
    let settled = settle(journal, id);
    let failure = match &settled {
        Ok(b) => b.ops.iter().find_map(|op| match &op.outcome {
            Outcome::Skipped(skip) => {
                let to = op.plan["to"].as_str().or(op.plan["from"].as_str());
                Some(format!("could not place {}: {skip}", to.unwrap_or("?")))
            }
            Outcome::Done(_) => None,
        }),
        Err(e) => Some(format!("the journal failed: {e}")),
    };
    if let (Some(why), Ok(b)) = (&failure, &settled) {
        tracing::warn!(%why, "could not place the whole group; rolling it back");
        if b.summary.done > 0 {
            roll_back(journal, id, origin, &actor);
        }
    }
    // What is in place now: all of it, or, after a failure, what the disk shows.
    let root = library.root();
    let in_place: Vec<&(&Item, String)> = match failure {
        None => placed.iter().collect(),
        Some(_) => placed
            .iter()
            .filter(|(item, to)| !item.staged.exists() && root.join(to).symlink_metadata().is_ok())
            .collect(),
    };
    if !in_place.is_empty() {
        staging.keep_folders();
        // The renames are on disk before the sessions' data, the files' other copy, goes.
        sync_dir(&dir);
        for (item, rel) in &in_place {
            if let Err(e) = fs::remove_file(&item.data) {
                tracing::warn!(path = %rel, error = %e, "could not remove a placed upload's data");
            }
        }
    }
    let complete = in_place.len() == placed.len();
    let result = UploadCommitResult::Placed {
        files: in_place
            .iter()
            .map(|(item, rel)| PlacedFile {
                session: item.session.clone(),
                path: rel.clone(),
            })
            .collect(),
        warnings,
    };
    // Each session keeps the result, so a repeated commit gets it again (see
    // `earlier_commit`) and never places the files twice. A group placed in part answers
    // only for the sessions it placed; the others stay, to be committed again.
    let mut sessions: Vec<String> = in_place.iter().map(|(i, _)| i.session.clone()).collect();
    if complete {
        sessions.extend(dropped.iter().cloned());
    }
    sessions.sort();
    let committed = Committed {
        sessions: sessions.clone(),
        result: result.clone(),
    };
    for session in &sessions {
        if let Err(e) = store.mark_committed(session, &committed) {
            // A repeated commit writes it, from the group's other tombstones; with none at
            // all it finds the session without its data (NotFound).
            tracing::error!(%session, error = %e, "could not record a committed upload");
        }
    }
    match failure {
        None => Ok(result),
        Some(why) if complete => {
            tracing::warn!(%why, "the journal failed, but the whole group is in place");
            Ok(result)
        }
        Some(why) => Err(UploadError::Io(why)),
    }
}

/// One `upload` per media file, with the sidecar uploaded with it, if any, so the two go
/// and come back together; a file of the group is a media file or the sidecar of one (see
/// `validate_group`).
fn upload_plans(root: &Path, placed: &[(&Item, String)]) -> Result<Vec<Planned>, UploadError> {
    let staged = |item: &Item, to: &str| -> Result<StagedFile, UploadError> {
        Ok(StagedFile {
            staged: rel_string(root, &item.staged).ok_or_else(|| {
                UploadError::Io(format!(
                    "could not place {to}: the staged name is not UTF-8"
                ))
            })?,
            to: to.to_string(),
            session: item.session.clone(),
            file_hash: item.file_hash.to_string(),
        })
    };
    placed
        .iter()
        .filter(|(item, _)| item.content.is_some())
        .map(|(media, to)| {
            let sidecar = placed.iter().find(|(item, _)| {
                item.content.is_none()
                    && media_path_for_sidecar(Path::new(&item.name))
                        .is_some_and(|m| m.as_os_str() == media.name.as_str())
            });
            Ok(Planned::new::<Upload>(&UploadPlan::Place {
                file: staged(media, to)?,
                sidecar: sidecar.map(|(s, to)| staged(s, to)).transpose()?,
            }))
        })
        .collect()
}

/// How long a commit waits for its batch to end once the Journal lost one of its records.
const SETTLE_LIMIT: Duration = Duration::from_secs(60);

/// Waits for the batch `id` to end, and gives its records. When the Journal could not keep
/// one of its records, `wait` gives that error at once although the batch may still run:
/// then this polls the batch's summary in the history until it is complete (at most
/// `SETTLE_LIMIT`), and gives the error.
fn settle(journal: &ServerJournal, id: BatchId) -> Result<BatchRecords, JournalError> {
    let e = match journal.wait(id) {
        Ok(b) => return Ok(b),
        Err(e) => e,
    };
    tracing::error!(batch = %id, error = %e, "the journal failed an upload commit");
    let deadline = Instant::now() + SETTLE_LIMIT;
    while !ended(journal, id) {
        if Instant::now() >= deadline {
            tracing::error!(batch = %id, "the upload batch did not end in time");
            break;
        }
        std::thread::sleep(POLL);
    }
    Err(e)
}

/// Whether the history has the batch `id` as ended.
fn ended(journal: &ServerJournal, id: BatchId) -> bool {
    let mut cursor = None;
    loop {
        let Ok(page) = journal.history(cursor, 64) else {
            return false;
        };
        if let Some(s) = page.iter().find(|s| s.begin.batch_id == id) {
            return s.complete;
        }
        match page.last() {
            Some(last) => cursor = Some(last.begin.batch_id),
            None => return false,
        }
    }
}

fn sorted_sessions(params: &UploadCommitParams) -> Vec<String> {
    let mut sessions: Vec<String> = params.files.iter().map(|f| f.session.clone()).collect();
    sessions.sort();
    sessions
}

/// The result of an earlier commit of exactly these sessions, if they are its tombstones.
/// Sessions of that commit that lack a tombstone (a crash while they were written) get one
/// now: the tombstones are written only after the whole group was placed. A commit naming
/// some committed sessions and not others, or not all of one earlier commit's, is `Invalid`.
fn earlier_commit(
    store: &UploadStore,
    user: UserId,
    params: &UploadCommitParams,
) -> Result<Option<UploadCommitResult>, UploadError> {
    let mut found = Vec::with_capacity(params.files.len());
    for f in &params.files {
        found.push(store.committed(user, &f.session)?);
    }
    let Some(first) = found.iter().flatten().next() else {
        return Ok(None);
    };
    let same =
        first.sessions == sorted_sessions(params) && found.iter().flatten().all(|c| c == first);
    if !same {
        return Err(UploadError::Invalid(
            "some of these uploads were committed already, in another group".into(),
        ));
    }
    for (f, c) in params.files.iter().zip(&found) {
        if c.is_none()
            && let Err(e) = store.mark_committed(&f.session, first)
        {
            tracing::error!(session = %f.session, error = %e, "could not record a committed upload");
        }
    }
    Ok(Some(first.result.clone()))
}

/// Checks every path and the group's shape; returns the group's folder (`""` for `Inbox/`
/// itself).
fn validate_group(params: &UploadCommitParams) -> Result<&str, UploadError> {
    let invalid = |m: String| Err(UploadError::Invalid(m));
    let Some(first) = params.files.first() else {
        return invalid("the group is empty".into());
    };
    for f in &params.files {
        validate_inbox_path(&f.path).map_err(UploadError::Invalid)?;
    }
    let folder = folder_of(&first.path);
    let (mut names, mut targets, mut sessions) = (HashSet::new(), HashSet::new(), HashSet::new());
    for f in &params.files {
        if folder_of(&f.path) != folder {
            return invalid("all files of a group must be in one folder".into());
        }
        // Compared as placed: `x.jpg.XMP` and `x.jpg.xmp` would both be `x.jpg.xmp`.
        if !targets.insert(target_name(file_name(&f.path), 1)) {
            return invalid(format!("path {:?} appears twice", f.path));
        }
        names.insert(file_name(&f.path));
        if !sessions.insert(f.session.as_str()) {
            return invalid(format!("session {:?} appears twice", f.session));
        }
    }
    for name in names.iter().filter(|n| is_sidecar_name(n)) {
        let media = media_path_for_sidecar(Path::new(name));
        let matches = media
            .as_ref()
            .and_then(|m| m.to_str())
            .is_some_and(|m| names.contains(m) && !is_sidecar_name(m));
        if !matches {
            return invalid(format!("sidecar {name:?} belongs to no media of the group"));
        }
    }
    Ok(folder)
}

fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn is_sidecar_name(name: &str) -> bool {
    asteria_common_core::paths::is_sidecar(Path::new(name))
}

/// `name` for `n == 1`; otherwise ` (n)` goes before the last extension (`a.b (2).HEIC`).
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// Where the group's file `name` goes under number `n`. A sidecar follows its media and is
/// spelled with a lowercase `.xmp`, the only spelling the library reads
/// (`IMG.HEIC.XMP` -> `IMG (2).HEIC.xmp`).
fn target_name(name: &str, n: u32) -> String {
    match media_path_for_sidecar(Path::new(name)) {
        Some(media) => sidecar_name(&numbered(&media.to_string_lossy(), n)),
        None => numbered(name, n),
    }
}

/// The sidecar name of media file `media`.
fn sidecar_name(media: &str) -> String {
    sidecar_path_for(Path::new(media))
        .to_string_lossy()
        .into_owned()
}

/// The first `n` under which no file of the group is taken in `dir`, nor the sidecar name
/// of any media file (a stray sidecar there would be adopted by the new media).
fn free_number(dir: &Path, items: &[Item]) -> Result<u32, UploadError> {
    let names: Vec<String> = items
        .iter()
        .flat_map(|i| {
            let sidecar = i.content.is_some().then(|| sidecar_name(&i.name));
            std::iter::once(i.name.clone()).chain(sidecar)
        })
        .collect();
    (1..=MAX_NUMBER)
        .find(|&n| {
            names
                .iter()
                .all(|name| dir.join(target_name(name, n)).symlink_metadata().is_err())
        })
        .ok_or_else(|| UploadError::Invalid("too many files with these names".into()))
}

/// Takes the placed files of the batch `id` back out of the library, into the trash, by
/// undoing it, and waits for the undo to end; then the [`Staging`] removes the folders made
/// for the group. Their content is still in the sessions, so a retry places them again. What
/// the undo could not take back, the commit answers for as placed.
fn roll_back(journal: &ServerJournal, id: BatchId, origin: &Origin, actor: &JournalActor) {
    let undone = journal
        .undo_batch(id, origin, actor)
        .and_then(|u| settle(journal, u));
    match undone {
        Ok(b) if b.summary.skipped == 0 => {}
        Ok(b) => {
            tracing::error!(batch = %id, skipped = b.summary.skipped, "could not take back every placed upload");
        }
        Err(e) => {
            tracing::error!(batch = %id, error = %e, "could not take back the placed uploads")
        }
    }
}

fn journal_error(e: JournalError) -> UploadError {
    match e {
        JournalError::Busy => UploadError::Busy("the journal is busy".into()),
        e => {
            tracing::error!(error = %e, "the journal failed an upload commit");
            UploadError::Io(e.to_string())
        }
    }
}

fn fw(e: FwError) -> UploadError {
    UploadError::Io(e.to_string())
}

fn server(e: ServerError) -> UploadError {
    tracing::error!(error = %e, "cache error during upload commit");
    UploadError::Io(e.to_string())
}

/// Lets a test stop a commit right before it hashes a given session, or act right before a
/// file is renamed into place (on the Journal's thread).
#[cfg(test)]
pub(crate) mod hold {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex};

    type PlaceHook = Arc<dyn Fn(&Path) + Send + Sync>;

    /// The hooks, by the library root they act below.
    static PLACE_HOOKS: Mutex<Vec<(PathBuf, PlaceHook)>> = Mutex::new(Vec::new());

    /// Removes its library's hook when dropped.
    pub struct PlaceHookGuard(PathBuf);

    impl Drop for PlaceHookGuard {
        fn drop(&mut self) {
            PLACE_HOOKS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(root, _)| root != &self.0);
        }
    }

    /// Calls `f` with each target below `root` right before a file is placed there.
    pub fn on_place(root: &Path, f: impl Fn(&Path) + Send + Sync + 'static) -> PlaceHookGuard {
        PLACE_HOOKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((root.to_path_buf(), Arc::new(f)));
        PlaceHookGuard(root.to_path_buf())
    }

    pub fn before_place(target: &Path) {
        let hooks: Vec<PlaceHook> = PLACE_HOOKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(root, _)| target.starts_with(root))
            .map(|(_, f)| f.clone())
            .collect();
        for f in hooks {
            f(target);
        }
    }

    pub struct Hold {
        pub session: String,
        pub entered: Sender<()>,
        pub release: Mutex<Receiver<()>>,
    }

    pub static HOLD: Mutex<Option<Arc<Hold>>> = Mutex::new(None);
    /// Taken by each test that sets `HOLD`, so that they do not run at once.
    pub static TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub fn before_hash(session: &str) {
        let hold = HOLD.lock().unwrap().clone();
        if let Some(h) = hold
            && h.session == session
        {
            let _ = h.entered.send(());
            let _ = h.release.lock().unwrap().recv();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asteria_common_api::UploadFile;

    fn origin() -> Origin {
        Origin::new("test")
    }

    /// The library's journal, as the server opens it.
    fn journal_for(lib: &Arc<Library>, cache: &Arc<Cache>) -> ServerJournal {
        struct Media;
        impl crate::AppExtension for Media {
            fn accepts(&self, kind: asteria_common_core::MediaKind) -> bool {
                matches!(
                    kind,
                    asteria_common_core::MediaKind::Image | asteria_common_core::MediaKind::Video
                )
            }
        }
        let ctx = crate::journal::JournalCtx {
            library: lib.clone(),
            cache: cache.clone(),
            ext: Arc::new(Media),
        };
        crate::journal::open_journal(ctx, None).unwrap()
    }

    /// A writer over the library, with its journal.
    fn writer_for(
        lib: &Arc<Library>,
        cache: &Arc<Cache>,
    ) -> (crate::Writer, tokio::task::JoinHandle<()>) {
        crate::spawn_writer(lib.clone(), cache.clone(), journal_for(lib, cache))
    }

    #[test]
    fn numbering() {
        assert_eq!(numbered("IMG_0001.HEIC", 1), "IMG_0001.HEIC");
        assert_eq!(target_name("IMG_0001.HEIC.xmp", 2), "IMG_0001 (2).HEIC.xmp");
        assert_eq!(target_name("a.b.HEIC.XMP", 2), "a.b (2).HEIC.xmp");
        assert_eq!(target_name("a.b.HEIC.Xmp", 1), "a.b.HEIC.xmp");
        assert_eq!(target_name("a.b.HEIC", 3), "a.b (3).HEIC");
        assert_eq!(numbered("README", 3), "README (3)");
        // Before the last extension, not the first '.'.
        assert_eq!(numbered("a.b.HEIC", 2), "a.b (2).HEIC");
        assert_eq!(
            numbered("Screenshot 2026-10-26 at 10.15.30.png", 2),
            "Screenshot 2026-10-26 at 10.15.30 (2).png"
        );
    }

    #[test]
    fn failure_midway_leaves_no_partial_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let journal = journal_for(&lib, &cache);
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let upload = |bytes: &[u8]| {
            let tmp = dir.path().join("hash-me");
            fs::write(&tmp, bytes).unwrap();
            let s = store
                .begin(me, bytes.len() as u64, file_hash(&tmp).unwrap())
                .unwrap()
                .session;
            store.put_chunk(me, &s, 0, bytes).unwrap();
            s
        };
        let (heic, mov) = (upload(b"heic bytes"), upload(b"mov bytes"));
        let params = UploadCommitParams {
            files: vec![
                UploadFile {
                    session: heic.clone(),
                    path: "a/b/IMG_0001.HEIC".into(),
                },
                UploadFile {
                    session: mov.clone(),
                    path: "a/b/IMG_0001.MOV".into(),
                },
            ],
            allow_duplicate: false,
        };
        // Someone takes the second name after numbering chose it.
        let hook = hold::on_place(lib.root(), |target: &Path| {
            if target.ends_with("IMG_0001.MOV") {
                fs::create_dir(target).unwrap();
            }
        });
        let r = commit(
            &lib,
            &cache,
            &journal,
            &store,
            me,
            &origin(),
            params.clone(),
        );
        drop(hook);
        assert!(matches!(r, Err(UploadError::Io(_))), "{r:?}");
        assert!(!root.join("Inbox/a/b/IMG_0001.HEIC").exists());
        // Both sessions are whole again, so the client can retry.
        for s in [&heic, &mov] {
            assert!(store.completed(me, s).is_ok(), "{s}");
        }
        assert_eq!(
            fs::read(store.completed(me, &heic).unwrap().data_path).unwrap(),
            b"heic bytes"
        );
        // Only the squatter's directory is left; the retry numbers around it.
        let r = commit(&lib, &cache, &journal, &store, me, &origin(), params).unwrap();
        let UploadCommitResult::Placed { files, .. } = r else {
            panic!("{r:?}")
        };
        assert_eq!(files[0].path, "Inbox/a/b/IMG_0001 (2).HEIC");
    }

    /// The session data is on another filesystem than the library (EXDEV): the files are
    /// copied into place, and the sessions end as tombstones without their data.
    #[test]
    fn cross_device_commit_copies_the_files_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let journal = journal_for(&lib, &cache);
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let upload = |bytes: &[u8]| {
            let tmp = dir.path().join("hash-me");
            fs::write(&tmp, bytes).unwrap();
            let s = store
                .begin(me, bytes.len() as u64, file_hash(&tmp).unwrap())
                .unwrap()
                .session;
            store.put_chunk(me, &s, 0, bytes).unwrap();
            s
        };
        let (heic, mov) = (upload(b"heic bytes"), upload(b"mov bytes"));
        let data = |s: &str| dir.path().join("state/uploads").join(s).join("data");
        let params = UploadCommitParams {
            files: vec![
                UploadFile {
                    session: heic.clone(),
                    path: "IMG_0001.HEIC".into(),
                },
                UploadFile {
                    session: mov.clone(),
                    path: "IMG_0001.MOV".into(),
                },
            ],
            allow_duplicate: false,
        };
        let r = crate::fsutil::test_hook::cross_device(|| {
            commit(
                &lib,
                &cache,
                &journal,
                &store,
                me,
                &origin(),
                params.clone(),
            )
        })
        .unwrap();
        assert!(matches!(r, UploadCommitResult::Placed { .. }), "{r:?}");
        assert_eq!(
            fs::read(root.join("Inbox/IMG_0001.HEIC")).unwrap(),
            b"heic bytes"
        );
        assert_eq!(
            fs::read(root.join("Inbox/IMG_0001.MOV")).unwrap(),
            b"mov bytes"
        );
        assert!(!data(&heic).exists() && !data(&mov).exists());
        let mut inbox: Vec<_> = fs::read_dir(root.join("Inbox"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        inbox.sort();
        assert_eq!(inbox, ["IMG_0001.HEIC", "IMG_0001.MOV"]);
        // A repeated commit gets the same answer from the tombstones.
        assert_eq!(
            commit(&lib, &cache, &journal, &store, me, &origin(), params).unwrap(),
            r
        );
    }

    #[test]
    fn cross_device_failure_midway_leaves_no_partial_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let journal = journal_for(&lib, &cache);
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let upload = |bytes: &[u8]| {
            let tmp = dir.path().join("hash-me");
            fs::write(&tmp, bytes).unwrap();
            let s = store
                .begin(me, bytes.len() as u64, file_hash(&tmp).unwrap())
                .unwrap()
                .session;
            store.put_chunk(me, &s, 0, bytes).unwrap();
            s
        };
        let (heic, mov) = (upload(b"heic bytes"), upload(b"mov bytes"));
        #[cfg(unix)]
        let inode = |s: &str| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(store.completed(me, s).unwrap().data_path)
                .unwrap()
                .ino()
        };
        #[cfg(unix)]
        let heic_inode = inode(&heic);
        let params = UploadCommitParams {
            files: vec![
                UploadFile {
                    session: heic.clone(),
                    path: "IMG_0001.HEIC".into(),
                },
                UploadFile {
                    session: mov.clone(),
                    path: "IMG_0001.MOV".into(),
                },
            ],
            allow_duplicate: false,
        };
        // Someone takes the second name after numbering chose it.
        let _hook = hold::on_place(lib.root(), |target: &Path| {
            if target.ends_with("IMG_0001.MOV") {
                fs::write(target, b"theirs").unwrap();
            }
        });
        let r = crate::fsutil::test_hook::cross_device(|| {
            commit(&lib, &cache, &journal, &store, me, &origin(), params)
        });
        assert!(matches!(r, Err(UploadError::Io(_))), "{r:?}");
        let inbox: Vec<_> = fs::read_dir(root.join("Inbox"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(inbox, ["IMG_0001.MOV"]);
        assert_eq!(
            fs::read(root.join("Inbox/IMG_0001.MOV")).unwrap(),
            b"theirs"
        );
        for (s, bytes) in [(&heic, &b"heic bytes"[..]), (&mov, &b"mov bytes"[..])] {
            let done = store.completed(me, s).unwrap();
            assert_eq!(fs::read(done.data_path).unwrap(), bytes);
        }
        // The copy was removed, not moved back over the session's own data (which a real
        // cross-device rename could not do).
        #[cfg(unix)]
        assert_eq!(inode(&heic), heic_inode);
    }

    #[test]
    fn rollback_removes_the_folders_it_created() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let journal = journal_for(&lib, &cache);
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let upload = |bytes: &[u8]| {
            let tmp = dir.path().join("hash-me");
            fs::write(&tmp, bytes).unwrap();
            let s = store
                .begin(me, bytes.len() as u64, file_hash(&tmp).unwrap())
                .unwrap()
                .session;
            store.put_chunk(me, &s, 0, bytes).unwrap();
            s
        };
        let (heic, mov) = (upload(b"heic bytes"), upload(b"mov bytes"));
        let params = UploadCommitParams {
            files: vec![
                UploadFile {
                    session: heic.clone(),
                    path: "a/b/IMG_0001.HEIC".into(),
                },
                UploadFile {
                    session: mov.clone(),
                    path: "a/b/IMG_0001.MOV".into(),
                },
            ],
            allow_duplicate: false,
        };
        // The second file's staged copy vanishes before it is renamed into place.
        let staged_mov = mov.clone();
        let _hook = hold::on_place(lib.root(), move |target: &Path| {
            if target.ends_with("IMG_0001.MOV") {
                fs::remove_file(tmp_path_for(&target.parent().unwrap().join(&staged_mov))).unwrap();
            }
        });
        let r = commit(&lib, &cache, &journal, &store, me, &origin(), params);
        assert!(matches!(r, Err(UploadError::Io(_))), "{r:?}");
        assert!(store.completed(me, &heic).is_ok());
        assert!(!root.join("Inbox").exists());
    }

    /// `Inbox/`, or a folder below it, that is a link fails the commit before anything is
    /// placed: the files would land outside the library (#32).
    #[cfg(unix)]
    #[test]
    fn an_upload_never_goes_through_a_linked_folder() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let journal = journal_for(&lib, &cache);
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let commit = |path: &str| {
            let tmp = dir.path().join("hash-me");
            fs::write(&tmp, path.as_bytes()).unwrap();
            let s = store
                .begin(me, path.len() as u64, file_hash(&tmp).unwrap())
                .unwrap()
                .session;
            store.put_chunk(me, &s, 0, path.as_bytes()).unwrap();
            let params = UploadCommitParams {
                files: vec![UploadFile {
                    session: s.clone(),
                    path: path.into(),
                }],
                allow_duplicate: false,
            };
            (
                commit(&lib, &cache, &journal, &store, me, &origin(), params),
                s,
            )
        };

        symlink(&outside, root.join("Inbox")).unwrap();
        let (r, s) = commit("IMG_0001.JPG");
        assert!(
            matches!(&r, Err(UploadError::Invalid(m)) if m.contains("\"Inbox\"")),
            "{r:?}"
        );
        assert!(store.completed(me, &s).is_ok());

        fs::remove_file(root.join("Inbox")).unwrap();
        fs::create_dir_all(root.join("Inbox/trip")).unwrap();
        symlink(&outside, root.join("Inbox/trip/day1")).unwrap();
        let (r, _) = commit("trip/day1/more/IMG_0002.JPG");
        assert!(
            matches!(&r, Err(UploadError::Invalid(m)) if m.contains("Inbox/trip/day1")),
            "{r:?}"
        );
        assert!(fs::read_dir(&outside).unwrap().next().is_none());

        let (r, _) = commit("trip/day2/IMG_0003.JPG");
        assert!(matches!(r, Ok(UploadCommitResult::Placed { .. })), "{r:?}");
        assert!(root.join("Inbox/trip/day2/IMG_0003.JPG").is_file());
    }

    #[test]
    fn sessions_are_sealed_during_the_commit_and_kept_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let journal = journal_for(&lib, &cache);
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let upload = |bytes: &[u8]| {
            let tmp = dir.path().join("hash-me");
            fs::write(&tmp, bytes).unwrap();
            let s = store
                .begin(me, bytes.len() as u64, file_hash(&tmp).unwrap())
                .unwrap()
                .session;
            store.put_chunk(me, &s, 0, bytes).unwrap();
            s
        };
        let jpg = upload(b"jpeg bytes");
        let xmp = upload(b"<not xmp");
        let params = UploadCommitParams {
            files: vec![
                UploadFile {
                    session: jpg.clone(),
                    path: "x.jpg".into(),
                },
                UploadFile {
                    session: xmp.clone(),
                    path: "x.jpg.xmp".into(),
                },
            ],
            allow_duplicate: false,
        };
        // What the hook found, as it runs on the Journal's thread.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook = {
            let (store, seen) = (store.clone(), seen.clone());
            let sessions = [
                (jpg.clone(), &b"jpeg bytes"[..]),
                (xmp.clone(), &b"<not xmp"[..]),
            ];
            hold::on_place(lib.root(), move |target: &Path| {
                for (s, body) in &sessions {
                    let chunk = store.put_chunk(me, s, 0, body);
                    let abort = store.abort(me, s);
                    seen.lock().unwrap().push(format!("{chunk:?} {abort:?}"));
                }
                // Then fail the move.
                fs::write(target, b"squatter").unwrap();
            })
        };
        let r = commit(&lib, &cache, &journal, &store, me, &origin(), params);
        drop(hook);
        assert!(matches!(r, Err(UploadError::Io(_))), "{r:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        for s in seen.iter() {
            assert!(
                s.starts_with("Err(Busy(") && s.contains(" Err(Busy("),
                "{s}"
            );
        }
        // Unsealed again, and the dropped sidecar's session survives the failed commit.
        for s in [&jpg, &xmp] {
            assert!(store.completed(me, s).is_ok(), "{s}");
            store
                .put_chunk(
                    me,
                    s,
                    0,
                    &fs::read(store.completed(me, s).unwrap().data_path).unwrap(),
                )
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn other_writes_go_on_while_a_commit_hashes() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;

        use asteria_common_api::MediaUpdateParams;
        use asteria_common_core::MediaKind;

        struct Photos;
        impl crate::AppExtension for Photos {
            fn accepts(&self, kind: MediaKind) -> bool {
                matches!(kind, MediaKind::Image | MediaKind::Video)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../asteria-common-core/tests/fixtures/red.jpg"),
            root.join("a.jpg"),
        )
        .unwrap();
        crate::scan(&lib, &cache, &Photos).unwrap();
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let tmp = dir.path().join("hash-me");
        fs::write(&tmp, b"heic bytes").unwrap();
        let session = store
            .begin(me, 10, file_hash(&tmp).unwrap())
            .unwrap()
            .session;
        store.put_chunk(me, &session, 0, b"heic bytes").unwrap();
        let (writer, _h) = writer_for(&lib, &cache);

        let _serial = hold::TESTS.lock().await;
        // The commit stops as it starts hashing, as a big file would keep it there.
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        *hold::HOLD.lock().unwrap() = Some(Arc::new(hold::Hold {
            session: session.clone(),
            entered,
            release: Mutex::new(release_rx),
        }));
        let committing = {
            let (writer, store) = (writer.clone(), store.clone());
            let params = UploadCommitParams {
                files: vec![UploadFile {
                    session: session.clone(),
                    path: "IMG_0001.HEIC".into(),
                }],
                allow_duplicate: false,
            };
            tokio::spawn(async move { writer.commit_upload(store, me, origin(), params).await })
        };
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(20)))
            .await
            .unwrap()
            .expect("the commit reached its hashing");

        let id = cache.get_by_path("a.jpg").unwrap().unwrap().id;
        let update = MediaUpdateParams {
            id,
            rating: Some(3),
            subjects: None,
            hierarchical_subjects: None,
            description: None,
            label: None,
        };
        let updated = tokio::time::timeout(
            Duration::from_secs(5),
            writer.update(
                Actor::User(me),
                corundum_framework::journal::Origin::new("test"),
                update,
            ),
        )
        .await;
        release.send(()).unwrap();
        *hold::HOLD.lock().unwrap() = None;
        let rec = updated
            .expect("media.update waited for the commit's hashing")
            .unwrap();
        assert_eq!(rec.sidecar.owner_rating, Some(3));
        let r = committing.await.unwrap().unwrap();
        assert!(matches!(r, UploadCommitResult::Placed { .. }), "{r:?}");
        assert!(root.join("Inbox/IMG_0001.HEIC").is_file());
    }

    /// The client gives up on a slow commit (its request future is dropped): the commit
    /// still finishes, and asking again gets its answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_commit_finishes_after_its_caller_gave_up() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let tmp = dir.path().join("hash-me");
        fs::write(&tmp, b"heic bytes").unwrap();
        let session = store
            .begin(me, 10, file_hash(&tmp).unwrap())
            .unwrap()
            .session;
        store.put_chunk(me, &session, 0, b"heic bytes").unwrap();
        let (writer, _h) = writer_for(&lib, &cache);
        let params = UploadCommitParams {
            files: vec![UploadFile {
                session: session.clone(),
                path: "IMG_0001.HEIC".into(),
            }],
            allow_duplicate: false,
        };

        let _serial = hold::TESTS.lock().await;
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        *hold::HOLD.lock().unwrap() = Some(Arc::new(hold::Hold {
            session: session.clone(),
            entered,
            release: Mutex::new(release_rx),
        }));
        let request = {
            let (writer, store, params) = (writer.clone(), store.clone(), params.clone());
            tokio::spawn(async move { writer.commit_upload(store, me, origin(), params).await })
        };
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(20)))
            .await
            .unwrap()
            .expect("the commit reached its hashing");
        // The request goes away (a client timeout drops it) while the commit hashes.
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        // Meanwhile a repeated commit finds the sessions busy.
        let again = writer
            .commit_upload(store.clone(), me, origin(), params.clone())
            .await;
        assert!(matches!(again, Err(UploadError::Busy(_))), "{again:?}");
        release.send(()).unwrap();
        *hold::HOLD.lock().unwrap() = None;

        // Nobody asks again: the abandoned commit places the file and leaves its tombstone
        // on its own.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while store.committed(me, &session).unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the abandoned commit did not finish"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let r = writer
            .commit_upload(store.clone(), me, origin(), params)
            .await
            .unwrap();
        let UploadCommitResult::Placed { files, .. } = r else {
            panic!("{r:?}")
        };
        assert_eq!(files[0].path, "Inbox/IMG_0001.HEIC");
        assert_eq!(
            fs::read(root.join("Inbox/IMG_0001.HEIC")).unwrap(),
            b"heic bytes"
        );
        assert!(store.committed(me, &session).unwrap().is_some());
    }

    /// The files are staged by the first phase, outside the queue; a commit that does not
    /// place them removes them, and the folders made for them.
    #[test]
    fn staged_files_that_are_not_placed_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let journal = journal_for(&lib, &cache);
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let tmp = dir.path().join("hash-me");
        fs::write(&tmp, b"jpeg bytes").unwrap();
        let s = store
            .begin(me, 10, file_hash(&tmp).unwrap())
            .unwrap()
            .session;
        store.put_chunk(me, &s, 0, b"jpeg bytes").unwrap();
        let params = UploadCommitParams {
            files: vec![UploadFile {
                session: s.clone(),
                path: "trip/x.jpg".into(),
            }],
            allow_duplicate: false,
        };
        let names = |d: &Path| -> Vec<String> {
            fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        };
        for cross in [false, true] {
            let prepared = if cross {
                crate::fsutil::test_hook::cross_device(|| prepare(&lib, &store, me, params.clone()))
            } else {
                prepare(&lib, &store, me, params.clone())
            };
            let prepared = prepared.unwrap();
            let Prepare::Ready(p) = prepared else {
                panic!("not prepared")
            };
            let staged = names(&root.join("Inbox/trip"));
            assert_eq!(staged.len(), 1, "{staged:?}");
            assert!(staged[0].ends_with(".asteria-tmp"), "{staged:?}");
            drop(p);
            assert!(!root.join("Inbox").exists());
            assert!(store.completed(me, &s).is_ok());
        }

        // Duplicates found in the queue: nothing placed, nothing left staged.
        fs::create_dir(root.join("Inbox")).unwrap();
        fs::write(root.join("Inbox/old.jpg"), b"jpeg bytes").unwrap();
        crate::scan(&lib, &cache, &AllMedia).unwrap();
        crate::hash_pending(&lib, &cache, &std::sync::atomic::AtomicBool::new(false)).unwrap();
        let r = commit(&lib, &cache, &journal, &store, me, &origin(), params).unwrap();
        assert!(matches!(r, UploadCommitResult::Duplicate { .. }), "{r:?}");
        assert_eq!(names(&root.join("Inbox")), ["old.jpg"]);
        assert!(store.completed(me, &s).is_ok());
    }

    /// A store that cannot keep the first `fail` operation records it is given, as a full
    /// disk would.
    struct Losing {
        inner: corundum_framework::journal::JsonlStore,
        fail: usize,
    }

    impl corundum_framework::journal::JournalStore for Losing {
        fn append(
            &mut self,
            record: &corundum_framework::journal::Record,
        ) -> Result<(), corundum_framework::journal::StoreError> {
            if matches!(record, corundum_framework::journal::Record::Op(_)) && self.fail > 0 {
                self.fail -= 1;
                return Err(corundum_framework::journal::StoreError::Io(
                    "no space left (test)".into(),
                ));
            }
            self.inner.append(record)
        }

        fn batches_rev(
            &self,
            cursor: Option<BatchId>,
            limit: usize,
        ) -> Result<
            Vec<corundum_framework::journal::BatchSummary>,
            corundum_framework::journal::StoreError,
        > {
            self.inner.batches_rev(cursor, limit)
        }

        fn batch(
            &self,
            id: BatchId,
        ) -> Result<
            Option<corundum_framework::journal::BatchRecords>,
            corundum_framework::journal::StoreError,
        > {
            self.inner.batch(id)
        }
    }

    /// The Journal loses a record of the commit's batch, which still places every file: the
    /// commit waits for the batch, answers with what was placed and leaves its tombstones, so
    /// a retry gets the answer and places nothing twice.
    #[test]
    fn a_lost_journal_record_does_not_place_the_group_twice() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let ctx = crate::journal::JournalCtx {
            library: lib.clone(),
            cache: cache.clone(),
            ext: Arc::new(AllMedia),
        };
        let journal =
            crate::journal::open_journal_over(ctx, None, |inner| Losing { inner, fail: 1 })
                .unwrap();
        let store = Arc::new(UploadStore::open(&dir.path().join("state")).unwrap());
        let me = lib.owner();
        let upload = |bytes: &[u8]| {
            let tmp = dir.path().join("hash-me");
            fs::write(&tmp, bytes).unwrap();
            let s = store
                .begin(me, bytes.len() as u64, file_hash(&tmp).unwrap())
                .unwrap()
                .session;
            store.put_chunk(me, &s, 0, bytes).unwrap();
            s
        };
        let (heic, mov) = (upload(b"heic bytes"), upload(b"mov bytes"));
        let params = UploadCommitParams {
            files: vec![
                UploadFile {
                    session: heic,
                    path: "IMG_0001.HEIC".into(),
                },
                UploadFile {
                    session: mov,
                    path: "IMG_0001.MOV".into(),
                },
            ],
            allow_duplicate: false,
        };
        let first = commit(
            &lib,
            &cache,
            &journal,
            &store,
            me,
            &origin(),
            params.clone(),
        );
        let again = commit(&lib, &cache, &journal, &store, me, &origin(), params);
        let mut inbox: Vec<_> = fs::read_dir(root.join("Inbox"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        inbox.sort();
        assert_eq!(inbox, ["IMG_0001.HEIC", "IMG_0001.MOV"]);
        let first = first.unwrap();
        assert!(
            matches!(first, UploadCommitResult::Placed { .. }),
            "{first:?}"
        );
        assert_eq!(again.unwrap(), first);
        let batches = journal.history(None, 100).unwrap();
        assert_eq!(batches.len(), 1, "{batches:?}");
    }

    struct AllMedia;
    impl crate::AppExtension for AllMedia {
        fn accepts(&self, _: asteria_common_core::MediaKind) -> bool {
            true
        }
    }
}
