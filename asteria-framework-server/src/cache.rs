//! Rebuildable redb cache. It is never a source of truth: if it cannot be opened, has the
//! wrong schema or belongs to another library, it is deleted and recreated.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::ops::Bound;
use std::path::Path;

use asteria_framework::{LibraryId, Sidecar};
use asteria_framework_api::{ItemId, MediaKind};
use grain_id::GrainId;
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
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

fn ce(e: impl Display) -> ServerError {
    ServerError::Cache(e.to_string())
}

pub struct Cache {
    db: Database,
    epoch: String,
}

impl Cache {
    /// Open (or create) `<dir>/cache.redb`. Any failure to open, a schema mismatch or a
    /// library mismatch deletes the file and starts over with a new epoch.
    pub fn open(dir: &Path, library_id: &LibraryId) -> Result<Cache, ServerError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        match Self::open_checked(&path, library_id) {
            Ok(c) => Ok(c),
            Err(_) => {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                Self::open_checked(&path, library_id)
            }
        }
    }

    fn open_checked(path: &Path, library_id: &LibraryId) -> Result<Cache, ServerError> {
        let db = Database::create(path).map_err(ce)?;
        let lib = library_id.0.hyphenated().to_string();
        let w = db.begin_write().map_err(ce)?;
        let epoch;
        {
            w.open_table(ITEMS).map_err(ce)?;
            w.open_table(IDS).map_err(ce)?;
            w.open_table(UNPROCESSABLE).map_err(ce)?;
            let mut meta = w.open_table(META).map_err(ce)?;
            let existing_schema = meta.get("schema").map_err(ce)?.map(|v| v.value().to_vec());
            match existing_schema {
                None => {
                    epoch = GrainId::random().to_string();
                    meta.insert("schema", CACHE_SCHEMA.to_le_bytes().as_slice())
                        .map_err(ce)?;
                    meta.insert("library_id", lib.as_bytes()).map_err(ce)?;
                    meta.insert("epoch", epoch.as_bytes()).map_err(ce)?;
                }
                Some(schema) => {
                    if schema != CACHE_SCHEMA.to_le_bytes() {
                        return Err(ServerError::Cache("schema mismatch".into()));
                    }
                    let stored_lib = meta
                        .get("library_id")
                        .map_err(ce)?
                        .map(|v| v.value().to_vec());
                    if stored_lib.as_deref() != Some(lib.as_bytes()) {
                        return Err(ServerError::Cache("library id mismatch".into()));
                    }
                    let e = meta.get("epoch").map_err(ce)?.map(|v| v.value().to_vec());
                    epoch = String::from_utf8(e.ok_or_else(|| ce("missing epoch"))?).map_err(ce)?;
                }
            }
        }
        w.commit().map_err(ce)?;
        Ok(Cache { db, epoch })
    }

    pub fn epoch(&self) -> String {
        self.epoch.clone()
    }

    /// Insert or replace a record, keeping the id index consistent.
    pub fn put(&self, rec: &ItemRecord) -> Result<(), ServerError> {
        let bytes = serde_json::to_vec(rec).map_err(ce)?;
        let w = self.db.begin_write().map_err(ce)?;
        {
            let mut items = w.open_table(ITEMS).map_err(ce)?;
            let mut ids = w.open_table(IDS).map_err(ce)?;
            let old = items
                .insert(rec.path.as_str(), bytes.as_slice())
                .map_err(ce)?
                .and_then(|v| serde_json::from_slice::<ItemRecord>(v.value()).ok());
            if let Some(old) = old
                && old.id != rec.id
            {
                ids.remove(old.id.0.as_str()).map_err(ce)?;
            }
            ids.insert(rec.id.0.as_str(), rec.path.as_str())
                .map_err(ce)?;
        }
        w.commit().map_err(ce)
    }

    pub fn remove(&self, rel: &str) -> Result<(), ServerError> {
        let w = self.db.begin_write().map_err(ce)?;
        {
            let mut items = w.open_table(ITEMS).map_err(ce)?;
            let mut ids = w.open_table(IDS).map_err(ce)?;
            let old = items
                .remove(rel)
                .map_err(ce)?
                .and_then(|v| serde_json::from_slice::<ItemRecord>(v.value()).ok());
            if let Some(old) = old {
                ids.remove(old.id.0.as_str()).map_err(ce)?;
            }
        }
        w.commit().map_err(ce)
    }

    /// Read failures count as a miss: the cache is rebuildable.
    pub fn get_by_path(&self, rel: &str) -> Option<ItemRecord> {
        let r = self.db.begin_read().ok()?;
        let items = r.open_table(ITEMS).ok()?;
        let v = items.get(rel).ok()??;
        serde_json::from_slice(v.value()).ok()
    }

    pub fn get_by_id(&self, id: &ItemId) -> Option<ItemRecord> {
        let r = self.db.begin_read().ok()?;
        let path = r
            .open_table(IDS)
            .ok()?
            .get(id.0.as_str())
            .ok()??
            .value()
            .to_string();
        let items = r.open_table(ITEMS).ok()?;
        let v = items.get(path.as_str()).ok()??;
        serde_json::from_slice(v.value()).ok()
    }

    pub fn all_paths(&self) -> Vec<String> {
        let Ok(r) = self.db.begin_read() else {
            return Vec::new();
        };
        let Ok(items) = r.open_table(ITEMS) else {
            return Vec::new();
        };
        let Ok(iter) = items.iter() else {
            return Vec::new();
        };
        iter.filter_map(|e| e.ok().map(|(k, _)| k.value().to_string()))
            .collect()
    }

    pub fn count(&self) -> u64 {
        self.db
            .begin_read()
            .ok()
            .and_then(|r| r.open_table(ITEMS).ok())
            .and_then(|t| t.len().ok())
            .unwrap_or(0)
    }

    /// Direct children of `folder` ("" = root): `(sub-folder names, items in path order,
    /// next cursor)`. The cursor is the last returned path; pass it as `after`.
    /// Folder names are complete on the first page (`after == None`) and empty afterwards.
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
    ) -> (Vec<String>, Vec<ItemRecord>, Option<String>) {
        self.try_list_folder(folder, after, limit)
            .unwrap_or_default()
    }

    fn try_list_folder(
        &self,
        folder: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Option<(Vec<String>, Vec<ItemRecord>, Option<String>)> {
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
        let r = self.db.begin_read().ok()?;
        let items = r.open_table(ITEMS).ok()?;

        let mut folders: Vec<String> = Vec::new();
        let mut out: Vec<ItemRecord> = Vec::new();
        let mut more = false;
        // Next scan position. `after` is exclusive; jumps are inclusive.
        let mut lower: Bound<String> = match after {
            Some(a) if a >= prefix.as_str() => Bound::Excluded(a.to_string()),
            _ => Bound::Included(prefix.clone()),
        };
        loop {
            let hi = upper.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
            let lo = match &lower {
                Bound::Included(s) => Bound::Included(s.as_str()),
                Bound::Excluded(s) => Bound::Excluded(s.as_str()),
                Bound::Unbounded => Bound::Unbounded,
            };
            let mut iter = items.range::<&str>((lo, hi)).ok()?;
            let mut jump: Option<String> = None;
            for entry in iter.by_ref() {
                let (k, v) = entry.ok()?;
                let key = k.value();
                let rest = &key[prefix.len()..];
                if let Some(slash) = rest.find('/') {
                    let name = &rest[..slash];
                    folders.push(name.to_string());
                    jump = Some(format!("{prefix}{name}0"));
                    break;
                }
                if out.len() < limit {
                    out.push(serde_json::from_slice(v.value()).ok()?);
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
        if !first_page {
            folders.clear();
        }
        let next = if more {
            out.last().map(|r| r.path.clone())
        } else {
            None
        };
        Some((folders, out, next))
    }

    pub fn mark_unprocessable(
        &self,
        hash: &str,
        preset: &str,
        reason: &str,
    ) -> Result<(), ServerError> {
        let w = self.db.begin_write().map_err(ce)?;
        {
            let mut t = w.open_table(UNPROCESSABLE).map_err(ce)?;
            t.insert(format!("{hash}|{preset}").as_str(), reason)
                .map_err(ce)?;
        }
        w.commit().map_err(ce)
    }

    pub fn unprocessable(&self, hash: &str, preset: &str) -> Option<String> {
        let r = self.db.begin_read().ok()?;
        let t = r.open_table(UNPROCESSABLE).ok()?;
        let v = t.get(format!("{hash}|{preset}").as_str()).ok()??;
        Some(v.value().to_string())
    }
}
