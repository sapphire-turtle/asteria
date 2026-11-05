use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::UserId;
use crate::error::{Error, Result};
use crate::hash::{ContentHash, HashScope};

use super::xml;

/// A rating and when it was last changed (if known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RatingEntry {
    /// `-1..=5`; `-1` is Lightroom's "rejected".
    pub value: i8,
    pub modified: Option<DateTime<Utc>>,
}

/// The parsed document, kept so unknown content can be written back.
#[derive(Debug, Clone)]
pub(crate) struct XmpDoc {
    pub(crate) root: xml::Element,
    pub(crate) xpacket: bool,
}

/// What asteria knows about one media file's XMP sidecar.
#[derive(Debug, Clone, Default)]
pub struct Sidecar {
    /// `xmp:Rating` plus `asteria:RatingModified`.
    pub rating: Option<RatingEntry>,
    /// `asteria:UserRatings`.
    pub user_ratings: BTreeMap<UserId, RatingEntry>,
    /// `dc:subject` (Bag).
    pub subjects: BTreeSet<String>,
    /// `lr:hierarchicalSubject` (Bag, levels separated by `|`).
    pub hierarchical_subjects: BTreeSet<String>,
    /// `dc:description` (Alt, `x-default`).
    pub description: Option<String>,
    /// `xmp:Label`.
    pub label: Option<String>,
    /// `asteria:ContentHash` and `asteria:HashScope`.
    pub content_hash: Option<(ContentHash, HashScope)>,
    /// `asteria:FileHash`: the `file/1` hash of the whole media file, so only the value is kept.
    pub file_hash: Option<ContentHash>,
    pub(crate) doc: Option<XmpDoc>,
}

impl Sidecar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any managed field other than the content hash holds data.
    pub fn has_user_data(&self) -> bool {
        self.rating.is_some()
            || !self.user_ratings.is_empty()
            || !self.subjects.is_empty()
            || !self.hierarchical_subjects.is_empty()
            || self.description.is_some()
            || self.label.is_some()
    }

    /// Sets `user`'s rating (`owner`'s goes to the main rating), last writer wins by `at`.
    ///
    /// An existing entry with a strictly newer `modified` wins; on equal timestamps the
    /// greater value wins. Returns whether anything changed; out of range is `InvalidRating`.
    pub fn apply_rating(
        &mut self,
        owner: UserId,
        user: UserId,
        value: i8,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        if !(-1..=5).contains(&value) {
            return Err(Error::InvalidRating(value));
        }
        let new = RatingEntry {
            value,
            modified: Some(at),
        };
        let slot = if user == owner {
            &mut self.rating
        } else {
            return Ok(match self.user_ratings.get_mut(&user) {
                Some(e) => replace_if_newer(e, new),
                None => {
                    self.user_ratings.insert(user, new);
                    true
                }
            });
        };
        Ok(match slot {
            Some(e) => replace_if_newer(e, new),
            None => {
                *slot = Some(new);
                true
            }
        })
    }

    /// Whether the source file was wrapped in `<?xpacket ...?>`.
    pub fn has_xpacket_wrapper(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.xpacket)
    }
}

/// Replaces `existing` unless it wins (newer time, then greater value); returns whether it changed.
fn replace_if_newer(existing: &mut RatingEntry, new: RatingEntry) -> bool {
    let wins = existing
        .modified
        .zip(new.modified)
        .is_some_and(|(m, at)| m > at || (m == at && existing.value >= new.value));
    if wins || *existing == new {
        return false;
    }
    *existing = new;
    true
}
