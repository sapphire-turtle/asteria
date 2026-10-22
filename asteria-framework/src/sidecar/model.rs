use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::UserId;
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
    pub(crate) doc: Option<XmpDoc>,
}

impl Sidecar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the source file was wrapped in `<?xpacket ...?>`.
    pub fn has_xpacket_wrapper(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.xpacket)
    }
}
