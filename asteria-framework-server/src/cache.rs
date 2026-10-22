//! Rebuildable redb cache. It is never a source of truth: if it cannot be opened, has the
//! wrong schema or belongs to another library, it is deleted and recreated.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::ops::Bound;
use std::path::Path;

use asteria_framework::{LibraryId, Sidecar};
use asteria_framework_api::{ItemId, MediaKind};
use grain_id::GrainId;
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
    WriteTransaction,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ServerError;

pub const CACHE_SCHEMA: u32 = 1;

const FILE_NAME: &str = "cache.redb";
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const ITEMS: TableDefinition<&str, &[u8]> = TableDefinition::new("items");
const IDS: TableDefinition<&str, &str> = TableDefinition::new("ids");
const UNPROCESSABLE: TableDefinition<&str, &str> = TableDefinition::new("unprocessable");

/// A content hash remembered with the (size, mtime) it was computed for.
/// Valid only while both still match the file on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredHash {
    pub hash: String,
    pub scope: String,
    pub size: u64,
    pub mtime_ns: i128,
}

/// `SidecarSnapshot::default()` means "no sidecar"; `From<&Sidecar>` is for an existing one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SidecarSnapshot {
    pub owner_rating: Option<i8>,
    pub user_ratings: BTreeMap<String, i8>,
    pub subjects: Vec<String>,
    pub hierarchical_subjects: Vec<String>,
    pub description: Option<String>,
    pub label: Option<String>,
    pub has_sidecar: bool,
}

impl From<&Sidecar> for SidecarSnapshot {
    fn from(s: &Sidecar) -> Self {
        SidecarSnapshot {
            owner_rating: s.rating.as_ref().map(|r| r.value),
            user_ratings: s
                .user_ratings
                .iter()
                .map(|(u, r)| (u.to_string(), r.value))
                .collect(),
            subjects: s.subjects.iter().cloned().collect(),
            hierarchical_subjects: s.hierarchical_subjects.iter().cloned().collect(),
            description: s.description.clone(),
            label: s.label.clone(),
            has_sidecar: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRecord {
    pub id: ItemId,
    pub path: String,
    pub kind: MediaKind,
    pub size: u64,
    pub mtime_ns: i128,
    pub hash: Option<StoredHash>,
    pub sidecar: SidecarSnapshot,
    pub sidecar_error: Option<String>,
}

/// Item ID: UUID v5, namespace = library UUID, name = library-relative path bytes.
pub fn item_id_for(library: &LibraryId, rel: &str) -> ItemId {
    ItemId(
        Uuid::new_v5(&library.0, rel.as_bytes())
            .hyphenated()
            .to_string(),
    )
}

/// `(sub-folder names, items, next cursor)` as returned by [`Cache::list_folder`].
pub type FolderPage = (Vec<String>, Vec<ItemRecord>, Option<String>);

fn ce(e: impl Display) -> ServerError {
    ServerError::Cache(e.to_string())
}

/// Why opening failed: damage that a rebuild fixes, or something that must propagate.
enum OpenError {
    Rebuild(String),
    Fatal(ServerError),
}

fn classify(e: impl Into<redb::Error>) -> OpenError {
    match e.into() {
        redb::Error::Corrupted(m) => OpenError::Rebuild(m),
        redb::Error::UpgradeRequired(v) => OpenError::Rebuild(format!("upgrade required ({v})")),
        // redb reports a file that is not a redb database as InvalidData.
        redb::Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            OpenError::Rebuild(e.to_string())
        }
        other => OpenError::Fatal(ce(other)),
    }
}

pub struct Cache {
    db: Database,
    epoch: String,
    library: LibraryId,
}

impl Cache {
    /// Open (or create) `<dir>/cache.redb`. A corrupted or old-format file, a schema
    /// mismatch or a library mismatch deletes the file and starts over with a new epoch.
    /// Other failures (database already open, I/O, permissions) propagate.
    pub fn open(dir: &Path, library_id: &LibraryId) -> Result<Cache, ServerError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        match Self::open_checked(&path, library_id) {
            Ok(c) => Ok(c),
            Err(OpenError::Fatal(e)) => Err(e),
            Err(OpenError::Rebuild(reason)) => {
                tracing::warn!(%reason, "rebuilding cache");
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                Self::open_checked(&path, library_id).map_err(|e| match e {
                    OpenError::Fatal(e) => e,
                    OpenError::Rebuild(m) => ce(m),
                })
            }
        }
    }

    fn open_checked(path: &Path, library_id: &LibraryId) -> Result<Cache, OpenError> {
        let db = Database::create(path).map_err(classify)?;
        let lib = library_id.0.hyphenated().to_string();
        let w = db.begin_write().map_err(classify)?;
        let epoch;
        {
            w.open_table(ITEMS).map_err(classify)?;
            w.open_table(IDS).map_err(classify)?;
            w.open_table(UNPROCESSABLE).map_err(classify)?;
            let mut meta = w.open_table(META).map_err(classify)?;
            let existing_schema = meta
                .get("schema")
                .map_err(classify)?
                .map(|v| v.value().to_vec());
            match existing_schema {
                None => {
                    epoch = GrainId::random().to_string();
                    meta.insert("schema", CACHE_SCHEMA.to_le_bytes().as_slice())
                        .map_err(classify)?;
                    meta.insert("library_id", lib.as_bytes())
                        .map_err(classify)?;
                    meta.insert("epoch", epoch.as_bytes()).map_err(classify)?;
                }
                Some(schema) => {
                    if schema != CACHE_SCHEMA.to_le_bytes() {
                        return Err(OpenError::Rebuild("schema mismatch".into()));
                    }
                    let stored_lib = meta
                        .get("library_id")
                        .map_err(classify)?
                        .map(|v| v.value().to_vec());
                    if stored_lib.as_deref() != Some(lib.as_bytes()) {
                        return Err(OpenError::Rebuild("library id mismatch".into()));
                    }
                    let e = meta
                        .get("epoch")
                        .map_err(classify)?
                        .map(|v| v.value().to_vec());
                    let e = e.ok_or_else(|| OpenError::Rebuild("missing epoch".into()))?;
                    epoch =
                        String::from_utf8(e).map_err(|_| OpenError::Rebuild("bad epoch".into()))?;
                }
            }
        }
        w.commit().map_err(classify)?;
        Ok(Cache {
            db,
            epoch,
            library: *library_id,
        })
    }

    pub fn epoch(&self) -> String {
        self.epoch.clone()
    }

    /// Write transactions are not fsynced individually (`Durability::None`); callers
    /// (the scanner) must call [`Cache::flush`] at the end of a batch. Anything not
    /// flushed may vanish after a crash, which is fine: the cache is rebuildable.
    fn write_txn(&self) -> Result<WriteTransaction, ServerError> {
        let mut w = self.db.begin_write().map_err(ce)?;
        w.set_durability(Durability::None).map_err(ce)?;
        Ok(w)
    }

    /// Make every earlier commit persistent (one Immediate-durability commit).
    pub fn flush(&self) -> Result<(), ServerError> {
        let w = self.db.begin_write().map_err(ce)?;
        w.commit().map_err(ce)
    }

    /// Insert or replace a record, keeping the id index consistent.
    pub fn put(&self, rec: &ItemRecord) -> Result<(), ServerError> {
        self.put_many(std::slice::from_ref(rec))
    }

    /// Insert or replace several records in one transaction.
    pub fn put_many(&self, recs: &[ItemRecord]) -> Result<(), ServerError> {
        let w = self.write_txn()?;
        {
            let mut items = w.open_table(ITEMS).map_err(ce)?;
            let mut ids = w.open_table(IDS).map_err(ce)?;
            for rec in recs {
                debug_assert_eq!(rec.id, item_id_for(&self.library, &rec.path));
                let bytes = serde_json::to_vec(rec).map_err(ce)?;
                let old = items
                    .insert(rec.path.as_str(), bytes.as_slice())
                    .map_err(ce)?
                    .map(|v| v.value().to_vec());
                if let Some(old) = old {
                    self.drop_old_id(&mut ids, &rec.path, &old)?;
                }
                ids.insert(rec.id.0.as_str(), rec.path.as_str())
                    .map_err(ce)?;
            }
        }
        w.commit().map_err(ce)
    }

    /// Sets only the `hash` of the record at `path`, re-reading the current record inside
    /// the write transaction. A non-`None` hash is stored only if its size and mtime still
    /// equal the record's. Returns whether anything was written.
    pub fn set_hash(&self, path: &str, hash: Option<StoredHash>) -> Result<bool, ServerError> {
        Ok(self.set_hash_many(&[(path.to_string(), hash)])? > 0)
    }

    /// [`Cache::set_hash`] for several paths in one transaction; returns how many were written.
    pub fn set_hash_many(
        &self,
        updates: &[(String, Option<StoredHash>)],
    ) -> Result<usize, ServerError> {
        self.update_records(updates, |rec, hash| {
            if hash
                .as_ref()
                .is_some_and(|h| h.size != rec.size || h.mtime_ns != rec.mtime_ns)
            {
                return false;
            }
            rec.hash = hash.clone();
            true
        })
    }

    /// Sets only the sidecar snapshot and sidecar error of the record at `path`.
    /// Returns whether the record exists.
    pub fn set_sidecar(
        &self,
        path: &str,
        snapshot: SidecarSnapshot,
        error: Option<String>,
    ) -> Result<bool, ServerError> {
        let n = self.update_records(&[(path.to_string(), (snapshot, error))], |rec, v| {
            rec.sidecar = v.0.clone();
            rec.sidecar_error = v.1.clone();
            true
        })?;
        Ok(n > 0)
    }

    /// Re-reads each record in a single write transaction and applies `f` to it; `f`
    /// returns whether to write it back. Missing or undecodable records are skipped.
    fn update_records<T>(
        &self,
        updates: &[(String, T)],
        f: impl Fn(&mut ItemRecord, &T) -> bool,
    ) -> Result<usize, ServerError> {
        let w = self.write_txn()?;
        let mut n = 0;
        {
            let mut items = w.open_table(ITEMS).map_err(ce)?;
            for (path, v) in updates {
                let cur = items.get(path.as_str()).map_err(ce)?.and_then(|g| {
                    serde_json::from_slice::<ItemRecord>(g.value())
                        .inspect_err(
                            |e| tracing::debug!(%path, error = %e, "skipping undecodable record"),
                        )
                        .ok()
                });
                let Some(mut rec) = cur else { continue };
                if f(&mut rec, v) {
                    let bytes = serde_json::to_vec(&rec).map_err(ce)?;
                    items.insert(path.as_str(), bytes.as_slice()).map_err(ce)?;
                    n += 1;
                }
            }
        }
        w.commit().map_err(ce)?;
        Ok(n)
    }

    pub fn remove(&self, rel: &str) -> Result<(), ServerError> {
        self.remove_many(&[rel.to_string()])
    }

    /// Remove several paths in one transaction.
    pub fn remove_many(&self, rels: &[String]) -> Result<(), ServerError> {
        let w = self.write_txn()?;
        {
            let mut items = w.open_table(ITEMS).map_err(ce)?;
            let mut ids = w.open_table(IDS).map_err(ce)?;
            for rel in rels {
                let old = items
                    .remove(rel.as_str())
                    .map_err(ce)?
                    .map(|v| v.value().to_vec());
                if let Some(old) = old {
                    self.drop_old_id(&mut ids, rel, &old)?;
                }
            }
        }
        w.commit().map_err(ce)
    }

    /// Remove the id entry of a replaced/removed record. If the old bytes cannot be
    /// decoded, recompute the id from the path so no stale entry survives.
    fn drop_old_id(
        &self,
        ids: &mut redb::Table<&str, &str>,
        path: &str,
        old: &[u8],
    ) -> Result<(), ServerError> {
        let id = match serde_json::from_slice::<ItemRecord>(old) {
            Ok(r) => r.id,
            Err(_) => item_id_for(&self.library, path),
        };
        ids.remove(id.0.as_str()).map_err(ce)?;
        Ok(())
    }

    pub fn get_by_path(&self, rel: &str) -> Result<Option<ItemRecord>, ServerError> {
        let r = self.db.begin_read().map_err(ce)?;
        let items = r.open_table(ITEMS).map_err(ce)?;
        match items.get(rel).map_err(ce)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value()).map_err(ce)?)),
            None => Ok(None),
        }
    }

    pub fn get_by_id(&self, id: &ItemId) -> Result<Option<ItemRecord>, ServerError> {
        let r = self.db.begin_read().map_err(ce)?;
        let path = match r
            .open_table(IDS)
            .map_err(ce)?
            .get(id.0.as_str())
            .map_err(ce)?
        {
            Some(p) => p.value().to_string(),
            None => return Ok(None),
        };
        let items = r.open_table(ITEMS).map_err(ce)?;
        match items.get(path.as_str()).map_err(ce)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value()).map_err(ce)?)),
            None => Ok(None),
        }
    }

    pub fn all_paths(&self) -> Result<Vec<String>, ServerError> {
        let r = self.db.begin_read().map_err(ce)?;
        let items = r.open_table(ITEMS).map_err(ce)?;
        let mut out = Vec::new();
        for e in items.iter().map_err(ce)? {
            let (k, _) = e.map_err(ce)?;
            out.push(k.value().to_string());
        }
        Ok(out)
    }

    pub fn count(&self) -> Result<u64, ServerError> {
        let r = self.db.begin_read().map_err(ce)?;
        r.open_table(ITEMS).map_err(ce)?.len().map_err(ce)
    }

    /// Direct children of `folder` ("" = root): `(sub-folder names sorted, items in path
    /// order, next cursor)`. The cursor is the last returned path; pass it as `after`.
    /// Folder names are complete on the first page (`after == None`) and empty afterwards.
    /// `limit` is clamped to at least 1. A cursor outside the folder yields an empty page.
    /// Undecodable records are skipped with a warning.
    ///
    /// Keys are rel paths, so descendants of `folder` form the contiguous range
    /// `["<folder>/", "<folder>0")` ('0' is the byte after '/'). A key whose remainder
    /// contains '/' lies in a sub-folder `n`; the scan then restarts at `"<folder>/n0"`,
    /// skipping the rest of that subtree without reading it.
    pub fn list_folder(
        &self,
        folder: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<FolderPage, ServerError> {
        validate_folder(folder)?;
        let limit = limit.max(1);
        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        let upper: Option<String> = if folder.is_empty() {
            None
        } else {
            Some(format!("{folder}0"))
        };
        let first_page = after.is_none();
        if let Some(a) = after
            && !a.starts_with(prefix.as_str())
        {
            return Ok((Vec::new(), Vec::new(), None));
        }
        let r = self.db.begin_read().map_err(ce)?;
        let items = r.open_table(ITEMS).map_err(ce)?;

        let mut folders: Vec<String> = Vec::new();
        let mut out: Vec<ItemRecord> = Vec::new();
        let mut more = false;
        // Next scan position. `after` is exclusive; jumps are inclusive.
        let mut lower: Bound<String> = match after {
            Some(a) => Bound::Excluded(a.to_string()),
            None => Bound::Included(prefix.clone()),
        };
        loop {
            let hi = upper.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
            let lo = match &lower {
                Bound::Included(s) => Bound::Included(s.as_str()),
                Bound::Excluded(s) => Bound::Excluded(s.as_str()),
                Bound::Unbounded => Bound::Unbounded,
            };
            let iter = items.range::<&str>((lo, hi)).map_err(ce)?;
            let mut jump: Option<String> = None;
            for entry in iter {
                let (k, v) = entry.map_err(ce)?;
                let key = k.value();
                let rest = &key[prefix.len()..];
                if let Some(slash) = rest.find('/') {
                    let name = &rest[..slash];
                    folders.push(name.to_string());
                    jump = Some(format!("{prefix}{name}0"));
                    break;
                }
                if out.len() < limit {
                    match serde_json::from_slice(v.value()) {
                        Ok(rec) => out.push(rec),
                        Err(e) => {
                            tracing::warn!(path = key, error = %e, "skipping undecodable cache record")
                        }
                    }
                } else {
                    more = true;
                    if !first_page {
                        break;
                    }
                }
            }
            match jump {
                Some(j) => lower = Bound::Included(j),
                None => break,
            }
            if more && !first_page {
                break;
            }
        }
        if first_page {
            folders.sort();
        } else {
            folders.clear();
        }
        let next = if more {
            out.last().map(|r| r.path.clone())
        } else {
            None
        };
        Ok((folders, out, next))
    }

    pub fn mark_unprocessable(
        &self,
        hash: &str,
        preset: &str,
        reason: &str,
    ) -> Result<(), ServerError> {
        let w = self.write_txn()?;
        {
            let mut t = w.open_table(UNPROCESSABLE).map_err(ce)?;
            t.insert(format!("{hash}|{preset}").as_str(), reason)
                .map_err(ce)?;
        }
        w.commit().map_err(ce)
    }

    pub fn unprocessable(&self, hash: &str, preset: &str) -> Result<Option<String>, ServerError> {
        let r = self.db.begin_read().map_err(ce)?;
        let t = r.open_table(UNPROCESSABLE).map_err(ce)?;
        let v = t
            .get(format!("{hash}|{preset}").as_str())
            .map_err(ce)?
            .map(|v| v.value().to_string());
        Ok(v)
    }
}

fn validate_folder(folder: &str) -> Result<(), ServerError> {
    if folder.is_empty() {
        return Ok(());
    }
    if folder
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(ServerError::InvalidPath(folder.to_string()));
    }
    Ok(())
}
