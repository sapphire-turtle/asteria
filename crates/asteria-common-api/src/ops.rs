//! `ops.apply`: a batch of rating operations a client queued while it was offline.

use serde::{Deserialize, Serialize};

use crate::{ItemId, MediaItem};

/// Most operations one `ops.apply` call may carry; more is `INVALID_PARAMS`.
pub const MAX_RATING_OPS: usize = 200;

/// What an operation does. Only ratings so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Rating,
}

/// One rating the client made at `at` (RFC 3339); the server merges it by time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatingOp {
    /// Chosen by the client; the result of this operation carries it back.
    pub id: String,
    pub kind: OpKind,
    pub item: ItemId,
    pub value: i8,
    pub at: String,
}

/// Parameters of `ops.apply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpsApplyParams {
    pub ops: Vec<RatingOp>,
}

/// What happened to one rating operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatingOpStatus {
    /// The rating changed.
    Applied,
    /// A rating at least as new was there already, or this same one was applied before.
    Superseded,
    /// Refused; `reason` says why.
    Rejected,
}

/// Why an operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    NotFound,
    /// The item's sidecar is malformed.
    ReadOnly,
    /// The value is out of range or `at` is not an RFC 3339 time.
    Invalid,
}

/// The outcome of one operation of an `ops.apply` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RatingOpResult {
    /// The id of the operation.
    pub id: String,
    pub status: RatingOpStatus,
    /// The item as it is now; absent when rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<MediaItem>,
    /// Present only when rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RejectReason>,
}

/// Result of `ops.apply`: one entry per operation, in the order sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpsApplyResult {
    pub results: Vec<RatingOpResult>,
}
