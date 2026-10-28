//! `upload.commit`: places a group of finished upload sessions under `Inbox/` (spec §4.7).
//!
//! Two phases, so that the heavy work never holds up the write queue:
//! - [`prepare`] seals the sessions, then checks and hashes them (and reads the sidecars)
//!   and stages each file in the group's folder: a hard link, or a synced copy when the
//!   sessions are on another filesystem than the library. All outside the queue;
//! - [`place`] runs in the write queue ([`crate::writer::Writer::commit_upload`]): it checks
//!   for duplicates again, numbers, renames the staged files into place, journals them and
//!   leaves the tombstones.
//!
//! The seals are held from the start of `prepare` to the end of `place`. Staged files that
//! were not placed (a failure, or duplicates found) are removed. The caller asks for a
//! rescan afterwards; this module does not touch the cache.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use asteria_framework::fsutil::tmp_path_for;
use asteria_framework::hash::{HashOutcome, HashScope, content_hash, file_hash};
use asteria_framework::journal::{Actor, JournalEntry, OpId, OpKind};
use asteria_framework::paths::{media_path_for_sidecar, sidecar_path_for};
use asteria_framework::upload_path::validate_inbox_path;
use asteria_framework::{Error as FwError, Library, Sidecar, UserId};
use asteria_framework_api::{PlacedFile, UploadCommitParams, UploadCommitResult, UploadWarning};
use chrono::Utc;
use grain_id::GrainId;

use crate::cache::Cache;
use crate::error::ServerError;
use crate::fsutil::{move_no_clobber, stage, sync_dir};
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
    fn new(dir: PathBuf, seals: Vec<SealGuard>) -> Result<Staging, UploadError> {
        let created_from = dir
            .ancestors()
            .take_while(|d| !d.exists())
            .last()
            .map(Path::to_path_buf);
        let staging = Staging {
            dir,
            created_from,
            files: Vec::new(),
            _seals: seals,
        };
        // On failure the drop removes what was made.
        fs::create_dir_all(&staging.dir)?;
        Ok(staging)
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

/// Places the group `params.files` under `Inbox/`: [`prepare`], then [`place`]. See spec §4.7
/// for the steps.
pub fn commit(
    library: &Library,
    cache: &Cache,
    store: &Arc<UploadStore>,
    user: UserId,
    params: UploadCommitParams,
) -> Result<UploadCommitResult, UploadError> {
    commit_with(library, cache, store, user, params, &mut |_| {})
}

/// [`commit`] with a hook called right before each file is moved to `target`.
fn commit_with(
    library: &Library,
    cache: &Cache,
    store: &Arc<UploadStore>,
    user: UserId,
    params: UploadCommitParams,
    before_place: &mut dyn FnMut(&Path),
) -> Result<UploadCommitResult, UploadError> {
    match prepare(library, store, user, params)? {
        Prepare::Answered(result) => Ok(result),
        Prepare::Ready(p) => place_with(library, cache, store, p, before_place),
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
        items.push((f.session.clone(), name, done.data_path, content));
    }

    // A sidecar that does not parse is dropped; the media still goes in.
    let mut warnings = Vec::new();
    let mut dropped = Vec::new();
    let mut keep = Vec::with_capacity(items.len());
    for (session, name, data, content) in items {
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
        keep.push((session, name, data, content));
    }

    // Staged on the library's filesystem now, so that placing them is only a rename.
    let dir = folder
        .split('/')
        .filter(|s| !s.is_empty())
        .fold(library.root().join(INBOX), |d, s| d.join(s));
    let mut staging = Staging::new(dir, seals)?;
    let mut kept = Vec::with_capacity(keep.len());
    for (session, name, data, content) in keep {
        let staged = tmp_path_for(&staging.dir.join(&session));
        stage(&data, &staged)
            .map_err(|e| UploadError::Io(format!("could not stage {name}: {e}")))?;
        staging.files.push(staged.clone());
        kept.push(Item {
            session,
            name,
            data,
            content,
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
/// then numbers the files, renames them into place, journals them and turns their sessions
/// into tombstones. Blocking; the seals are released when it returns.
pub fn place(
    library: &Library,
    cache: &Cache,
    store: &UploadStore,
    prepared: Prepared,
) -> Result<UploadCommitResult, UploadError> {
    place_with(library, cache, store, prepared, &mut |_| {})
}

fn place_with(
    library: &Library,
    cache: &Cache,
    store: &UploadStore,
    prepared: Prepared,
    before_place: &mut dyn FnMut(&Path),
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
    // Before anything is placed: a commit that cannot be journaled is not made.
    let journal = library.journal().map_err(fw)?;
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
    let mut placed: Vec<(&Item, PathBuf, String)> = Vec::with_capacity(kept.len());
    for item in &kept {
        let name = target_name(&item.name, n);
        let target = dir.join(&name);
        let rel = format!("{rel_dir}/{name}");
        before_place(&target);
        if let Err(e) = move_no_clobber(&item.staged, &target) {
            tracing::warn!(path = %rel, error = %e, "could not place upload; rolling back the group");
            roll_back(&placed);
            return Err(UploadError::Io(format!("could not place {rel}: {e}")));
        }
        placed.push((item, target, rel));
    }
    staging.keep_folders();
    // The renames are on disk before the sessions' data, the files' other copy, goes.
    sync_dir(&dir);
    for (item, _, rel) in &placed {
        if let Err(e) = fs::remove_file(&item.data) {
            tracing::warn!(path = %rel, error = %e, "could not remove a placed upload's data");
        }
    }

    for (item, _, rel) in &placed {
        let entry = JournalEntry {
            op_id: OpId(GrainId::random()),
            plan_id: None,
            actor: Actor::User(user),
            at: Utc::now(),
            kind: OpKind::Upload,
            before: serde_json::json!({ "session": item.session }),
            after: serde_json::json!({ "path": rel }),
        };
        // The file is in place already; failing the commit now would only invite a retry.
        if let Err(e) = journal.append(&entry) {
            tracing::error!(path = %rel, error = %e, "could not journal an upload");
        }
    }
    let result = UploadCommitResult::Placed {
        files: placed
            .iter()
            .map(|(item, _, rel)| PlacedFile {
                session: item.session.clone(),
                path: rel.clone(),
            })
            .collect(),
        warnings,
    };
    // Each session keeps the result, so a repeated commit gets it again (see `earlier_commit`).
    let committed = Committed {
        sessions: sorted_sessions(&params),
        result: result.clone(),
    };
    for session in placed.iter().map(|(i, _, _)| &i.session).chain(&dropped) {
        if let Err(e) = store.mark_committed(session, &committed) {
            // A repeated commit writes it, from the group's other tombstones; with none at
            // all it finds the session without its data (NotFound).
            tracing::error!(%session, error = %e, "could not record a committed upload");
        }
    }
    Ok(result)
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
    asteria_framework::paths::is_sidecar(Path::new(name))
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

/// Takes the placed files back out of the library. Their content is still in the sessions
/// (a staged file is a link or a copy), so they are only removed; the [`Staging`] then
/// removes the folders made for the group.
fn roll_back(placed: &[(&Item, PathBuf, String)]) {
    for (_, target, rel) in placed.iter().rev() {
        if let Err(e) = fs::remove_file(target) {
            tracing::error!(path = %rel, error = %e, "could not take back a placed upload");
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

/// Lets a test stop a commit right before it hashes a given session.
#[cfg(test)]
mod hold {
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex};

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
    use asteria_framework_api::UploadFile;

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
        let lib = Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state"));
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
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
        let mut hook = |target: &Path| {
            if target.ends_with("IMG_0001.MOV") {
                fs::create_dir(target).unwrap();
            }
        };
        let r = commit_with(&lib, &cache, &store, me, params.clone(), &mut hook);
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
        let r = commit(&lib, &cache, &store, me, params).unwrap();
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
        let lib = Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state"));
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
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
            commit(&lib, &cache, &store, me, params.clone())
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
        assert_eq!(commit(&lib, &cache, &store, me, params).unwrap(), r);
    }

    #[test]
    fn cross_device_failure_midway_leaves_no_partial_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state"));
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
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
        let mut hook = |target: &Path| {
            if target.ends_with("IMG_0001.MOV") {
                fs::write(target, b"theirs").unwrap();
            }
        };
        let r = crate::fsutil::test_hook::cross_device(|| {
            commit_with(&lib, &cache, &store, me, params, &mut hook)
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
        let lib = Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state"));
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
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
        let mut hook = |target: &Path| {
            if target.ends_with("IMG_0001.MOV") {
                fs::remove_file(tmp_path_for(&target.parent().unwrap().join(&mov))).unwrap();
            }
        };
        let r = commit_with(&lib, &cache, &store, me, params, &mut hook);
        assert!(matches!(r, Err(UploadError::Io(_))), "{r:?}");
        assert!(store.completed(me, &heic).is_ok());
        assert!(!root.join("Inbox").exists());
    }

    #[test]
    fn sessions_are_sealed_during_the_commit_and_kept_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state"));
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
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
        let mut hook = |target: &Path| {
            for (s, body) in [(&jpg, &b"jpeg bytes"[..]), (&xmp, &b"<not xmp"[..])] {
                let chunk = store.put_chunk(me, s, 0, body);
                assert!(matches!(chunk, Err(UploadError::Busy(_))), "{chunk:?}");
                assert!(matches!(store.abort(me, s), Err(UploadError::Busy(_))));
            }
            // Then fail the move.
            fs::write(target, b"squatter").unwrap();
        };
        let r = commit_with(&lib, &cache, &store, me, params, &mut hook);
        assert!(matches!(r, Err(UploadError::Io(_))), "{r:?}");
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

        use asteria_framework::MediaKind;
        use asteria_framework_api::MediaUpdateParams;

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
                .join("../asteria-framework/tests/fixtures/red.jpg"),
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
        let (writer, _h) = crate::spawn_writer(lib.clone(), cache.clone());

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
            tokio::spawn(async move { writer.commit_upload(store, me, params).await })
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
        let updated = tokio::time::timeout(Duration::from_secs(5), writer.update(me, update)).await;
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
        let (writer, _h) = crate::spawn_writer(lib.clone(), cache.clone());
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
            tokio::spawn(async move { writer.commit_upload(store, me, params).await })
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
            .commit_upload(store.clone(), me, params.clone())
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
            .commit_upload(store.clone(), me, params)
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
        let lib = Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state"));
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
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
        let r = commit(&lib, &cache, &store, me, params).unwrap();
        assert!(matches!(r, UploadCommitResult::Duplicate { .. }), "{r:?}");
        assert_eq!(names(&root.join("Inbox")), ["old.jpg"]);
        assert!(store.completed(me, &s).is_ok());
    }

    struct AllMedia;
    impl crate::AppExtension for AllMedia {
        fn accepts(&self, _: asteria_framework::MediaKind) -> bool {
            true
        }
    }
}
