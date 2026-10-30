//! Organize wire types (API 2.3): undoable batches of move and trash operations.

use serde::{Deserialize, Serialize};

use crate::ItemId;

/// Most operations one `organize.apply` batch may carry; more is `INVALID_PARAMS`.
pub const MAX_OPS: usize = 10_000;
/// Longest batch label, in characters.
pub const MAX_LABEL_CHARS: usize = 200;

/// One operation of an `organize.apply` batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizeOp {
    pub item_id: ItemId,
    #[serde(flatten)]
    pub action: OrganizeAction,
    /// `blake3:<hex>` the client saw; the op is skipped with `hash_mismatch` if the file differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_hash: Option<String>,
}

/// What an [`OrganizeOp`] does to its item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrganizeAction {
    /// `to` is library-relative with `/` separators.
    Move {
        to: String,
    },
    Trash,
}

/// Parameters of `organize.apply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizeApplyParams {
    pub ops: Vec<OrganizeOp>,
    #[serde(default)]
    pub label: Option<String>,
    /// Report what would happen without touching anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Parameters of `organize.undo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizeUndoParams {
    pub batch_id: String,
    #[serde(default)]
    pub dry_run: bool,
}

/// Result of `organize.apply` and `organize.undo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizeResult {
    /// `None` for a dry run.
    pub batch_id: Option<String>,
    pub results: Vec<OpResult>,
}

/// What happened to one operation. In a dry run `Done` means "can be done".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpResult {
    /// Position of the op in the request.
    pub index: u32,
    pub status: OpStatus,
    /// The original path.
    pub path: String,
    /// Where the file went; absent when skipped and for a trash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
    /// The item's new id; absent when skipped and for a trash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_item_id: Option<ItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SkipReason>,
}

/// Outcome of one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Done,
    Skipped,
}

/// Why an operation was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Missing,
    Changed,
    HashMismatch,
    TargetExists,
    Symlink,
    Io,
}

/// Parameters of `organize.history`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizeHistoryParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Result of `organize.history`, newest batch first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizeHistory {
    pub batches: Vec<BatchSummary>,
    pub next_cursor: Option<String>,
}

/// One batch in the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchSummary {
    pub batch_id: String,
    pub user: String,
    /// Whether an agent acted on behalf of `user`.
    pub agent: bool,
    /// RFC 3339.
    pub at: String,
    pub label: Option<String>,
    pub op_count: u32,
    pub done: u32,
    pub skipped: u32,
    /// Whether the batch ran to its end marker.
    pub complete: bool,
    /// The batch this one undid, if it is an undo.
    pub undo_of: Option<String>,
    /// The batch that undid this one, if any.
    pub undone_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LibraryInfo;

    #[test]
    fn ops_take_the_kind_inline() {
        let op: OrganizeOp =
            serde_json::from_str(r#"{"item_id":"x","kind":"move","to":"a/b.jpg"}"#).unwrap();
        assert_eq!(
            op.action,
            OrganizeAction::Move {
                to: "a/b.jpg".into()
            }
        );
        let op: OrganizeOp =
            serde_json::from_str(r#"{"item_id":"x","kind":"trash","file_hash":"blake3:00"}"#)
                .unwrap();
        assert_eq!(op.action, OrganizeAction::Trash);
        assert_eq!(op.file_hash.as_deref(), Some("blake3:00"));
    }

    #[test]
    fn dry_run_and_label_are_optional() {
        let p: OrganizeApplyParams = serde_json::from_str(r#"{"ops":[]}"#).unwrap();
        assert!(!p.dry_run);
        assert_eq!(p.label, None);
    }

    #[test]
    fn skip_reasons_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&SkipReason::TargetExists).unwrap(),
            r#""target_exists""#
        );
        assert_eq!(
            serde_json::to_string(&SkipReason::HashMismatch).unwrap(),
            r#""hash_mismatch""#
        );
    }

    #[test]
    fn library_info_without_revision_reads_as_zero() {
        let i: LibraryInfo = serde_json::from_str(
            r#"{"library_id":"l","owner":"o","epoch":"e","item_count":1,"scanning":false}"#,
        )
        .unwrap();
        assert_eq!(i.revision, 0);
    }
}
