//! The sidecar operations of `media.update` and `ops.apply`: `write_sidecar` sets tags,
//! description and label, `rate` sets one user's rating. Each writes the sidecar once and
//! refreshes the cache from it; the Journal records the values before and after.
//!
//! corundum runs an undo under the kind of the operation it reverses, so each plan also
//! takes its inverse's form: the values to set, with the values they must have now
//! (`expect`).

use std::collections::BTreeSet;
use std::path::Path;

use asteria_common_core::hash::{ContentHash, HashScope};
use asteria_common_core::{Error as FwError, Library, RatingEntry, Sidecar, UserId};
use chrono::DateTime;
use corundum_framework::journal::{Applied, OpKind, Operation, Registry, Skip};
use serde::{Deserialize, Serialize};

use super::JournalCtx;
use crate::cache::{Cache, ItemRecord, SidecarSnapshot, StoredHash};
use crate::error::ServerError;
use crate::scan::mtime_ns;

/// The fields `write_sidecar` sets; a field left `None` is not touched. Tags are
/// `subjects` and `hierarchical_subjects`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subjects: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchical_subjects: Option<BTreeSet<String>>,
    /// `Some(None)` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "present")]
    pub description: Option<Option<String>>,
    /// `Some(None)` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "present")]
    pub label: Option<Option<String>>,
}

impl Fields {
    pub fn is_empty(&self) -> bool {
        *self == Fields::default()
    }

    /// The values `sc` has for the fields this sets.
    fn current(&self, sc: &Sidecar) -> Fields {
        Fields {
            subjects: self.subjects.as_ref().map(|_| sc.subjects.clone()),
            hierarchical_subjects: self
                .hierarchical_subjects
                .as_ref()
                .map(|_| sc.hierarchical_subjects.clone()),
            description: self.description.as_ref().map(|_| sc.description.clone()),
            label: self.label.as_ref().map(|_| sc.label.clone()),
        }
    }

    /// `other`'s values for the fields this sets.
    fn within(&self, other: &Fields) -> Fields {
        Fields {
            subjects: self.subjects.as_ref().and(other.subjects.clone()),
            hierarchical_subjects: self
                .hierarchical_subjects
                .as_ref()
                .and(other.hierarchical_subjects.clone()),
            description: self.description.as_ref().and(other.description.clone()),
            label: self.label.as_ref().and(other.label.clone()),
        }
    }

    fn set(&self, sc: &mut Sidecar) {
        if let Some(v) = &self.subjects {
            sc.subjects = v.clone();
        }
        if let Some(v) = &self.hierarchical_subjects {
            sc.hierarchical_subjects = v.clone();
        }
        if let Some(v) = &self.description {
            sc.description = v.clone();
        }
        if let Some(v) = &self.label {
            sc.label = v.clone();
        }
    }
}

/// Sets `set` on the sidecar of `path`; in an undo, only if its fields still hold `expect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteSidecarPlan {
    pub path: String,
    pub set: Fields,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Fields>,
}

/// The fields `write_sidecar` set, before and after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteSidecarDone {
    pub path: String,
    pub before: Fields,
    pub after: Fields,
}

/// A user's rating and when it was set (`None`: the sidecar gives no time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatingAt {
    pub value: i8,
    pub at_ms: Option<i64>,
}

/// Rates `path` for `user`, last writer wins by `at_ms`. In an undo, `expect` is there: the
/// user's rating is then set to `rating` (`None` removes it) exactly, and only if it is
/// `expect` now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatePlan {
    pub path: String,
    pub user: UserId,
    #[serde(flatten)]
    pub rating: Option<RatingAt>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "present")]
    pub expect: Option<Option<RatingAt>>,
}

/// The user's rating before and after; equal when a newer rating won (superseded).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateDone {
    pub path: String,
    pub user: UserId,
    pub before: Option<RatingAt>,
    pub after: Option<RatingAt>,
}

/// `Option<Option<T>>` with a present `null` as `Some(None)` (and an absent field as `None`,
/// by `default`).
mod present {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(
        v: &Option<Option<T>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

pub struct WriteSidecar;
pub struct Rate;

impl OpKind for WriteSidecar {
    const KIND: &'static str = "write_sidecar";
    type Plan = WriteSidecarPlan;
    type Done = WriteSidecarDone;
}

impl OpKind for Rate {
    const KIND: &'static str = "rate";
    type Plan = RatePlan;
    type Done = RateDone;
}

/// Registers the sidecar operations.
pub(crate) fn register(reg: &mut Registry<JournalCtx>) {
    reg.register(WriteSidecar).register(Rate);
}

impl Operation<JournalCtx> for WriteSidecar {
    fn apply(
        &self,
        ctx: &JournalCtx,
        plan: &WriteSidecarPlan,
    ) -> Result<Applied<WriteSidecarDone>, Skip> {
        let (before, after) = write(ctx, &plan.path, |sc| {
            let before = plan.set.current(sc);
            if let Some(expect) = &plan.expect
                && before != plan.set.within(expect)
            {
                return Err(Skip::PreconditionFailed);
            }
            plan.set.set(sc);
            Ok((before, plan.set.current(sc)))
        })?;
        Ok(Applied::new(WriteSidecarDone {
            path: plan.path.clone(),
            before,
            after,
        }))
    }

    fn inverse(&self, done: &WriteSidecarDone) -> Option<WriteSidecarPlan> {
        Some(WriteSidecarPlan {
            path: done.path.clone(),
            set: done.before.clone(),
            expect: Some(done.after.clone()),
        })
    }
}

impl RatingAt {
    fn of(e: &RatingEntry) -> RatingAt {
        RatingAt {
            value: e.value,
            at_ms: e.modified.map(|t| t.timestamp_millis()),
        }
    }

    fn entry(self) -> RatingEntry {
        RatingEntry {
            value: self.value,
            modified: self.at_ms.and_then(DateTime::from_timestamp_millis),
        }
    }
}

/// `user`'s rating in `sc` (`owner`'s is the main one).
fn rating_of(sc: &Sidecar, owner: UserId, user: UserId) -> Option<RatingAt> {
    let e = if user == owner {
        sc.rating.as_ref()
    } else {
        sc.user_ratings.get(&user)
    };
    e.map(RatingAt::of)
}

fn set_rating(sc: &mut Sidecar, owner: UserId, user: UserId, r: Option<RatingAt>) {
    let entry = r.map(RatingAt::entry);
    if user == owner {
        sc.rating = entry;
    } else {
        match entry {
            Some(e) => {
                sc.user_ratings.insert(user, e);
            }
            None => {
                sc.user_ratings.remove(&user);
            }
        }
    }
}

impl Operation<JournalCtx> for Rate {
    fn apply(&self, ctx: &JournalCtx, plan: &RatePlan) -> Result<Applied<RateDone>, Skip> {
        let done = |before, after| {
            Ok(Applied::new(RateDone {
                path: plan.path.clone(),
                user: plan.user,
                before,
                after,
            }))
        };
        let owner = ctx.library.owner();
        let user = plan.user;
        if let Some(r) = plan.rating
            && !(-1..=5).contains(&r.value)
        {
            return Err(Skip::InvalidPlan(format!(
                "rating out of range: {}",
                r.value
            )));
        }
        match plan.expect {
            // An undo that changes nothing (of a superseded rating) needs no write.
            Some(expect) if expect == plan.rating => done(expect, expect),
            Some(expect) => {
                let (before, after) = write(ctx, &plan.path, |sc| {
                    let now = rating_of(sc, owner, user);
                    if now != expect {
                        return Err(Skip::PreconditionFailed);
                    }
                    set_rating(sc, owner, user, plan.rating);
                    Ok((now, rating_of(sc, owner, user)))
                })?;
                done(before, after)
            }
            None => {
                let Some(RatingAt {
                    value,
                    at_ms: Some(at_ms),
                }) = plan.rating
                else {
                    return Err(Skip::InvalidPlan(
                        "a rating needs a value and a time".into(),
                    ));
                };
                let at = DateTime::from_timestamp_millis(at_ms)
                    .ok_or_else(|| Skip::InvalidPlan(format!("time out of range: {at_ms}")))?;
                let (before, after) = write(ctx, &plan.path, |sc| {
                    let before = rating_of(sc, owner, user);
                    // The value's range was checked above.
                    let _ = sc.apply_rating(owner, user, value, at);
                    Ok((before, rating_of(sc, owner, user)))
                })?;
                done(before, after)
            }
        }
    }

    fn inverse(&self, done: &RateDone) -> Option<RatePlan> {
        Some(RatePlan {
            path: done.path.clone(),
            user: done.user,
            rating: done.before,
            expect: Some(done.after),
        })
    }
}

type Stamp = (u64, i128);

fn stat(path: &Path) -> Option<Stamp> {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len(), mtime_ns(&m)))
}

/// The cached hash, usable only if the file's current (size, mtime) equal the ones it was
/// computed for.
fn usable_hash(rec: &ItemRecord, now: Option<Stamp>) -> Option<(ContentHash, HashScope)> {
    let h: &StoredHash = rec.hash.as_ref()?;
    if now != Some((h.size, h.mtime_ns)) {
        return None;
    }
    Some((h.hash.parse().ok()?, h.scope.parse().ok()?))
}

/// The cached whole-file hash, under the same freshness rule as [`usable_hash`].
fn usable_file_hash(rec: &ItemRecord, now: Option<Stamp>) -> Option<ContentHash> {
    let h: &StoredHash = rec.hash.as_ref()?;
    if now != Some((h.size, h.mtime_ns)) {
        return None;
    }
    h.file_hash.parse().ok()
}

/// Runs `f` on the sidecar of `path` in one write, with the cached hashes when they are
/// fresh (no rehash), then refreshes the cache from the sidecar. `f` must leave the sidecar
/// as it was when it fails; nothing is written then.
fn write<T>(
    ctx: &JournalCtx,
    path: &str,
    f: impl FnOnce(&mut Sidecar) -> Result<T, Skip>,
) -> Result<T, Skip> {
    let (library, cache) = (&ctx.library, &ctx.cache);
    let rec = match cache.get_by_path(path) {
        Ok(Some(rec)) => rec,
        Ok(None) => return Err(Skip::NotFound),
        Err(e) => {
            tracing::error!(error = %e, "cache error during a sidecar write");
            return Err(Skip::Io(e.to_string()));
        }
    };
    let rel = Path::new(path);
    let stamp = stat(&library.root().join(rel));
    let hash = usable_hash(&rec, stamp);
    let file_hash = usable_file_hash(&rec, stamp);
    let mut out = None;
    let written = library.update_sidecar(rel, |sc| {
        if sc.content_hash.is_none() {
            sc.content_hash = hash;
        }
        sc.file_hash = file_hash;
        out = Some(f(sc));
    });
    if let Err(e @ (FwError::MediaNotFound(_) | FwError::InvalidPath(_))) = written {
        return Err(skip_of_fw(e, path));
    }
    // Refresh from disk after any write attempt, best effort.
    if let Err(e) = refresh_cache(library, cache, &rec) {
        tracing::warn!(error = %e, path, "cache refresh after write failed");
    }
    written.map_err(|e| skip_of_fw(e, path))?;
    out.unwrap_or(Err(Skip::Io("the sidecar was not read".into())))
}

/// `MalformedSidecar` is `Denied` (the item is read-only), and `TmpExists` (a write under
/// way) is `Exists`.
fn skip_of_fw(e: FwError, path: &str) -> Skip {
    match e {
        FwError::MediaNotFound(_) => Skip::NotFound,
        FwError::InvalidPath(_) => Skip::InvalidPath,
        FwError::MalformedSidecar { reason, .. } => {
            tracing::warn!(path, %reason, "sidecar write refused: malformed sidecar");
            Skip::Denied
        }
        FwError::TmpExists(_) => Skip::Exists,
        e @ FwError::UnsafeSidecarWrite(_) => {
            tracing::error!(error = %e, path, "unsafe sidecar write refused");
            Skip::Io(e.to_string())
        }
        e => {
            tracing::warn!(error = %e, path, "cannot write a sidecar");
            Skip::Io(e.to_string())
        }
    }
}

fn refresh_cache(library: &Library, cache: &Cache, rec: &ItemRecord) -> Result<(), ServerError> {
    let rel = Path::new(&rec.path);
    // The cache's StoredHash is filled only by hash_pending, never from the sidecar.
    match library.read_sidecar(rel) {
        Ok(Some(sc)) => {
            cache.set_sidecar(&rec.path, SidecarSnapshot::from(&sc), None)?;
        }
        Ok(None) => {
            cache.set_sidecar(&rec.path, SidecarSnapshot::default(), None)?;
        }
        Err(FwError::MalformedSidecar { reason, .. }) => {
            cache.set_sidecar(&rec.path, rec.sidecar.clone(), Some(reason))?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
