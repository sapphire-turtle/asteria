//! Sidecar update API: lazy creation, payload-hash recording, last-writer-wins ratings.

use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use grain_id::GrainId;
use serde_json::json;

use crate::error::{Error, Result};
use crate::hash::content_hash;
use crate::ids::UserId;
use crate::journal::{Actor, Journal, JournalEntry, OpId, OpKind};
use crate::library::Library;
use crate::paths::{META_DIR, is_sidecar, sidecar_path_for};
use crate::sidecar::{RatingEntry, Sidecar};

/// Rejects absolute paths, anything but plain components (`..`, `.`, prefixes), paths into
/// the library metadata directory, and sidecar paths.
fn checked_rel(media_rel: &Path) -> Result<()> {
    let ok = media_rel.components().next().is_some()
        && !is_sidecar(media_rel)
        && media_rel.components().next() != Some(Component::Normal(META_DIR.as_ref()))
        && media_rel
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidPath(media_rel.to_path_buf()))
    }
}

fn changed_fields(before: &Sidecar, after: &Sidecar) -> Vec<&'static str> {
    let mut out = Vec::new();
    if before.rating != after.rating {
        out.push("rating");
    }
    if before.user_ratings != after.user_ratings {
        out.push("user_ratings");
    }
    if before.subjects != after.subjects {
        out.push("subjects");
    }
    if before.hierarchical_subjects != after.hierarchical_subjects {
        out.push("hierarchical_subjects");
    }
    if before.description != after.description {
        out.push("description");
    }
    if before.label != after.label {
        out.push("label");
    }
    out
}

impl Library {
    /// Reads the sidecar of `media_rel` (relative to the library root), if any.
    pub fn read_sidecar(&self, media_rel: &Path) -> Result<Option<Sidecar>> {
        checked_rel(media_rel)?;
        Sidecar::read(&sidecar_path_for(&self.root().join(media_rel)))
    }

    /// Applies `f` to the sidecar of `media_rel`, creating it only when it gains content.
    ///
    /// `media_rel` must be plain components: empty and absolute paths, `..`, a leading `./`,
    /// paths under `.asteria` and sidecar paths are rejected with `InvalidPath`; a valid path
    /// that does not name an existing file is `MediaNotFound`. A change that only adds the
    /// content hash to an existing sidecar is not written.
    ///
    /// When `sc.content_hash` is `None` after `f` runs, the media file is hashed before writing.
    /// Callers that already know the hash can set `sc.content_hash` inside `f` to avoid hashing
    /// large files on the write path.
    ///
    /// Returns `Ok(true)` when a sidecar was written, `Ok(false)` when nothing needed writing.
    pub fn update_sidecar(
        &self,
        media_rel: &Path,
        actor: Actor,
        f: impl FnOnce(&mut Sidecar),
    ) -> Result<bool> {
        checked_rel(media_rel)?;
        let media: PathBuf = self.root().join(media_rel);
        if !media.is_file() {
            return Err(Error::MediaNotFound(media_rel.to_path_buf()));
        }
        let sidecar_path = sidecar_path_for(&media);
        let existing = Sidecar::read(&sidecar_path)?;
        let existed = existing.is_some();
        let mut sidecar = existing.unwrap_or_default();
        let original = sidecar.clone();
        f(&mut sidecar);

        let fields = changed_fields(&original, &sidecar);
        if fields.is_empty() && (existed || !sidecar.has_user_data()) {
            return Ok(false);
        }
        if sidecar.content_hash.is_none() {
            let outcome = content_hash(&media)?;
            sidecar.content_hash = Some((outcome.hash, outcome.scope));
        }
        sidecar.write(&sidecar_path)?;

        let path = media_rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let detail = json!({"path": path, "fields": fields});
        Journal::new(self).append(&JournalEntry {
            op_id: OpId(GrainId::random()),
            plan_id: None,
            actor,
            at: Utc::now(),
            kind: OpKind::WriteSidecar,
            before: detail.clone(),
            after: detail,
        })?;
        Ok(true)
    }

    /// Sets `user`'s rating (last writer wins by `at`).
    ///
    /// An existing entry with a strictly newer `modified` wins; on equal timestamps the greater
    /// value wins, so concurrent edits merge the same way in any order.
    pub fn set_rating(
        &self,
        media_rel: &Path,
        user: UserId,
        value: i8,
        at: DateTime<Utc>,
        actor: Actor,
    ) -> Result<()> {
        if !(-1..=5).contains(&value) {
            return Err(Error::InvalidRating(value));
        }
        let owner = self.owner();
        self.update_sidecar(media_rel, actor, |sc| {
            if user == owner {
                apply(&mut sc.rating, value, at);
            } else {
                let mut slot = sc.user_ratings.remove(&user);
                apply(&mut slot, value, at);
                if let Some(e) = slot {
                    sc.user_ratings.insert(user, e);
                }
            }
        })?;
        Ok(())
    }
}

/// Whether `existing` beats a new `(value, at)`: newer time wins, then the greater value.
fn existing_wins(existing: &RatingEntry, value: i8, at: DateTime<Utc>) -> bool {
    existing
        .modified
        .is_some_and(|m| m > at || (m == at && existing.value >= value))
}

fn apply(slot: &mut Option<RatingEntry>, value: i8, at: DateTime<Utc>) {
    if slot.as_ref().is_some_and(|e| existing_wins(e, value, at)) {
        return;
    }
    *slot = Some(RatingEntry {
        value,
        modified: Some(at),
    });
}
