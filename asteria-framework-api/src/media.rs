//! Gallery browse/update wire types.

use serde::{Deserialize, Serialize};

/// Coarse kind of a media file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Other,
}

/// Stable item identifier: a UUID v5 string, opaque to clients.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub String);

/// One media file as the gallery API presents it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: ItemId,
    /// Library-relative, `/`-separated.
    pub path: String,
    pub kind: MediaKind,
    pub size: u64,
    /// RFC 3339, UTC.
    pub modified: String,
    /// The requesting user's own rating.
    pub rating: Option<i8>,
    pub subjects: Vec<String>,
    pub hierarchical_subjects: Vec<String>,
    pub description: Option<String>,
    pub label: Option<String>,
    pub content_hash: Option<String>,
    pub read_only_reason: Option<String>,
}

/// Result of `library.info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryInfo {
    pub library_id: String,
    pub owner: String,
    pub epoch: String,
    pub item_count: u64,
    pub scanning: bool,
}

/// Parameters of `folder.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderListParams {
    /// Library-relative folder; `""` is the root.
    pub path: String,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// Result of `folder.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderListing {
    /// Names of the immediate subfolders.
    pub folders: Vec<String>,
    pub items: Vec<MediaItem>,
    pub next_cursor: Option<String>,
}

/// Parameters of `media.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaGetParams {
    pub id: ItemId,
}

/// Parameters of `media.update`. `None` leaves a field unchanged; for the nullable
/// fields, `Some(None)` clears it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaUpdateParams {
    pub id: ItemId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<i8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subjects: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchical_subjects: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<Option<String>>,
}

/// Present (even as null) becomes `Some`; absent stays `None` through `default`.
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_update_distinguishes_unset_and_clear() {
        let p: MediaUpdateParams =
            serde_json::from_str(r#"{"id":"x","description":null}"#).unwrap();
        assert_eq!(p.description, Some(None));
        assert_eq!(p.label, None);
        let p: MediaUpdateParams = serde_json::from_str(r#"{"id":"x","label":"a"}"#).unwrap();
        assert_eq!(p.label, Some(Some("a".into())));
        assert_eq!(p.description, None);
    }

    #[test]
    fn media_update_serialization_roundtrips() {
        let p = MediaUpdateParams {
            id: ItemId("x".into()),
            rating: Some(3),
            subjects: None,
            hierarchical_subjects: Some(vec!["a|b".into()]),
            description: Some(None),
            label: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(
            json,
            r#"{"id":"x","rating":3,"hierarchical_subjects":["a|b"],"description":null}"#
        );
        assert_eq!(serde_json::from_str::<MediaUpdateParams>(&json).unwrap(), p);
    }

    #[test]
    fn item_id_is_transparent() {
        assert_eq!(
            serde_json::to_string(&ItemId("u".into())).unwrap(),
            r#""u""#
        );
    }
}
