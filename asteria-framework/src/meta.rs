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
use crate::paths::sidecar_path_for;
use crate::sidecar::{RatingEntry, Sidecar};

/// Rejects absolute paths and anything but plain components (`..`, `.`, prefixes).
fn checked_rel(media_rel: &Path) -> Result<()> {
    let ok = media_rel.components().next().is_some()
        && media_rel
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(())
    } else {
        Err(Error::MediaNotFound(media_rel.to_path_buf()))
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

    /// Sets `user`'s rating; an existing entry with a strictly newer `modified` wins.
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
                let mut slot = sc.user_ratings.get(&user).cloned();
                apply(&mut slot, value, at);
                sc.user_ratings.extend(slot.map(|e| (user, e)));
            }
        })?;
        Ok(())
    }
}

fn newer_wins(existing: &RatingEntry, at: DateTime<Utc>) -> bool {
    existing.modified.is_some_and(|m| m > at)
}

fn apply(slot: &mut Option<RatingEntry>, value: i8, at: DateTime<Utc>) {
    if slot.as_ref().is_some_and(|e| newer_wins(e, at)) {
        return;
    }
    *slot = Some(RatingEntry {
        value,
        modified: Some(at),
    });
}
