//! `upload.commit`: places a group of finished upload sessions under `Inbox/` (spec §4.7).
//!
//! Runs inside the write queue ([`crate::writer::Writer::commit_upload`]). The caller asks
//! for a rescan afterwards; this module does not touch the cache.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use asteria_framework::hash::{HashOutcome, HashScope, content_hash, file_hash};
use asteria_framework::journal::{Actor, Journal, JournalEntry, OpId, OpKind};
use asteria_framework::paths::media_path_for_sidecar;
use asteria_framework::{Error as FwError, Library, Sidecar, UserId};
use asteria_framework_api::{PlacedFile, UploadCommitParams, UploadCommitResult, UploadWarning};
use chrono::Utc;
use grain_id::GrainId;

use crate::cache::Cache;
use crate::error::ServerError;
use crate::fsutil::move_no_clobber;
use crate::upload::{UploadError, UploadStore};

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
}

/// Places the group `params.files` under `Inbox/`. See spec §4.7 for the steps.
pub fn commit(
    library: &Library,
    cache: &Cache,
    store: &UploadStore,
    user: UserId,
    params: UploadCommitParams,
) -> Result<UploadCommitResult, UploadError> {
    commit_with(library, cache, store, user, params, &mut |_| {})
}

/// [`commit`] with a hook called right before each file is moved to `target`.
fn commit_with(
    library: &Library,
    cache: &Cache,
    store: &UploadStore,
    user: UserId,
    params: UploadCommitParams,
    before_place: &mut dyn FnMut(&Path),
) -> Result<UploadCommitResult, UploadError> {
    let folder = validate_group(&params)?;

    // Every session must be complete and match the hash claimed at `begin`.
    let mut items = Vec::with_capacity(params.files.len());
    for f in &params.files {
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
        items.push(Item {
            session: f.session.clone(),
            name,
            data: done.data_path,
            content,
        });
    }

    if !params.allow_duplicate {
        let mut existing = BTreeSet::new();
        for o in items.iter().filter_map(|i| i.content.as_ref()) {
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

    // A sidecar that does not parse is dropped; the media still goes in.
    let mut warnings = Vec::new();
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        if item.content.is_none() {
            match Sidecar::read(&item.data) {
                Ok(Some(_)) => {}
                Ok(None) => return Err(UploadError::NotFound),
                Err(FwError::MalformedSidecar { reason, .. }) => {
                    store.remove(&item.session);
                    warnings.push(UploadWarning::SidecarDropped {
                        session: item.session,
                        reason,
                    });
                    continue;
                }
                Err(e) => return Err(fw(e)),
            }
        }
        kept.push(item);
    }

    let dir = folder
        .split('/')
        .filter(|s| !s.is_empty())
        .fold(library.root().join(INBOX), |d, s| d.join(s));
    let rel_dir = if folder.is_empty() {
        INBOX.to_string()
    } else {
        format!("{INBOX}/{folder}")
    };
    let n = free_number(&dir, &kept)?;

    let created_from = dir
        .ancestors()
        .take_while(|d| !d.exists())
        .last()
        .map(Path::to_path_buf);
    fs::create_dir_all(&dir)?;
    let mut placed: Vec<(&Item, PathBuf, String)> = Vec::with_capacity(kept.len());
    for item in &kept {
        let name = numbered(&item.name, n);
        let target = dir.join(&name);
        let rel = format!("{rel_dir}/{name}");
        before_place(&target);
        if let Err(e) = move_no_clobber(&item.data, &target) {
            tracing::warn!(path = %rel, error = %e, "could not place upload; rolling back the group");
            roll_back(&placed, &dir, created_from.as_deref());
            return Err(UploadError::Io(format!("could not place {rel}: {e}")));
        }
        placed.push((item, target, rel));
    }

    let journal = Journal::new(library);
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
    for (item, _, _) in &placed {
        store.remove(&item.session);
    }

    Ok(UploadCommitResult::Placed {
        files: placed
            .into_iter()
            .map(|(item, _, rel)| PlacedFile {
                session: item.session.clone(),
                path: rel,
            })
            .collect(),
        warnings,
    })
}

/// Checks every path and the group's shape; returns the group's folder (`""` for `Inbox/`
/// itself).
fn validate_group(params: &UploadCommitParams) -> Result<&str, UploadError> {
    let invalid = |m: String| Err(UploadError::Invalid(m));
    let Some(first) = params.files.first() else {
        return invalid("the group is empty".into());
    };
    for f in &params.files {
        validate_path(&f.path)?;
    }
    let folder = folder_of(&first.path);
    let (mut names, mut sessions) = (HashSet::new(), HashSet::new());
    for f in &params.files {
        if folder_of(&f.path) != folder {
            return invalid("all files of a group must be in one folder".into());
        }
        if !names.insert(file_name(&f.path)) {
            return invalid(format!("path {:?} appears twice", f.path));
        }
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

/// `/`-separated, relative, every element non-empty and not starting with `.`; no `\`,
/// `:` or NUL anywhere.
fn validate_path(path: &str) -> Result<(), UploadError> {
    let invalid = |why: &str| Err(UploadError::Invalid(format!("path {path:?}: {why}")));
    if path.is_empty() {
        return invalid("empty");
    }
    if path.contains(['\\', ':', '\0']) {
        return invalid("contains '\\', ':' or NUL");
    }
    for element in path.split('/') {
        if element.is_empty() {
            return invalid("empty element");
        }
        if element.starts_with('.') {
            return invalid("element starts with '.'");
        }
    }
    Ok(())
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

/// `name` for `n == 1`; otherwise ` (n)` goes before the first `.`.
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_string();
    }
    match name.find('.') {
        Some(i) => format!("{} ({n}){}", &name[..i], &name[i..]),
        None => format!("{name} ({n})"),
    }
}

/// The first `n` under which no file of the group is taken in `dir`.
fn free_number(dir: &Path, items: &[Item]) -> Result<u32, UploadError> {
    (1..=MAX_NUMBER)
        .find(|&n| {
            items
                .iter()
                .all(|i| dir.join(numbered(&i.name, n)).symlink_metadata().is_err())
        })
        .ok_or_else(|| UploadError::Invalid("too many files with these names".into()))
}

/// Moves the placed files back into their sessions and removes the folders this commit
/// created (`created_from` and below), as far as they are empty.
fn roll_back(placed: &[(&Item, PathBuf, String)], dir: &Path, created_from: Option<&Path>) {
    for (item, target, rel) in placed.iter().rev() {
        if let Err(e) = fs::rename(target, &item.data) {
            tracing::error!(path = %rel, error = %e, "could not take back a placed upload");
        }
    }
    if let Some(top) = created_from {
        for d in dir.ancestors() {
            let _ = fs::remove_dir(d);
            if d == top {
                break;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use asteria_framework_api::UploadFile;

    #[test]
    fn numbering() {
        assert_eq!(numbered("IMG_0001.HEIC", 1), "IMG_0001.HEIC");
        assert_eq!(numbered("IMG_0001.HEIC.xmp", 2), "IMG_0001 (2).HEIC.xmp");
        assert_eq!(numbered("README", 3), "README (3)");
    }

    #[test]
    fn failure_midway_leaves_no_partial_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Library::init(&root, "owner").unwrap();
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
        let store = UploadStore::open(&root).unwrap();
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

    #[test]
    fn rollback_removes_the_folders_it_created() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        let lib = Library::init(&root, "owner").unwrap();
        let cache = Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap();
        let store = UploadStore::open(&root).unwrap();
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
        let mov_data = store.completed(me, &mov).unwrap().data_path;
        let params = UploadCommitParams {
            files: vec![
                UploadFile {
                    session: heic.clone(),
                    path: "a/b/IMG_0001.HEIC".into(),
                },
                UploadFile {
                    session: mov,
                    path: "a/b/IMG_0001.MOV".into(),
                },
            ],
            allow_duplicate: false,
        };
        // The second file vanishes from its session before it is moved.
        let mut hook = |target: &Path| {
            if target.ends_with("IMG_0001.MOV") {
                fs::remove_file(&mov_data).unwrap();
            }
        };
        let r = commit_with(&lib, &cache, &store, me, params, &mut hook);
        assert!(matches!(r, Err(UploadError::Io(_))), "{r:?}");
        assert!(store.completed(me, &heic).is_ok());
        assert!(!root.join("Inbox").exists());
    }
}
