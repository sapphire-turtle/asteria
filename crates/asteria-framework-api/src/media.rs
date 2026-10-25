//! Photos browse/update wire types.

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

/// One media file as the photos API presents it.
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
    /// BLAKE3 of the whole file (`blake3:<hex>`), `null` until computed. Changes whenever
    /// any byte of the file changes, metadata included; derived media are keyed on it.
    /// Absent from servers before API 2.1.
    #[serde(default)]
    pub file_hash: Option<String>,
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

    fn item(full: bool) -> MediaItem {
        MediaItem {
            id: ItemId("0d6a6f6e-6b0e-5d3b-9a43-2f1b5c3e8a10".into()),
            path: "2024/a.jpg".into(),
            kind: MediaKind::Image,
            size: 1234,
            modified: "2024-05-01T12:00:00Z".into(),
            rating: full.then_some(4),
            subjects: if full { vec!["cat".into()] } else { vec![] },
            hierarchical_subjects: if full {
                vec!["pets|cat".into()]
            } else {
                vec![]
            },
            description: full.then(|| "desc".into()),
            label: full.then(|| "Red".into()),
            content_hash: full.then(|| "blake3:abcd".into()),
            file_hash: full.then(|| "blake3:ef01".into()),
            read_only_reason: full.then(|| "bad xml".into()),
        }
    }

    const ITEM_EMPTY: &str = concat!(
        r#"{"id":"0d6a6f6e-6b0e-5d3b-9a43-2f1b5c3e8a10","path":"2024/a.jpg","kind":"image","#,
        r#""size":1234,"modified":"2024-05-01T12:00:00Z","rating":null,"subjects":[],"#,
        r#""hierarchical_subjects":[],"description":null,"label":null,"content_hash":null,"#,
        r#""file_hash":null,"read_only_reason":null}"#
    );

    const ITEM_FULL: &str = concat!(
        r#"{"id":"0d6a6f6e-6b0e-5d3b-9a43-2f1b5c3e8a10","path":"2024/a.jpg","kind":"image","#,
        r#""size":1234,"modified":"2024-05-01T12:00:00Z","rating":4,"subjects":["cat"],"#,
        r#""hierarchical_subjects":["pets|cat"],"description":"desc","label":"Red","#,
        r#""content_hash":"blake3:abcd","file_hash":"blake3:ef01","read_only_reason":"bad xml"}"#
    );

    #[test]
    fn media_item_wire_format_with_nulls() {
        let it = item(false);
        assert_eq!(serde_json::to_string(&it).unwrap(), ITEM_EMPTY);
        assert_eq!(serde_json::from_str::<MediaItem>(ITEM_EMPTY).unwrap(), it);
    }

    #[test]
    fn media_item_wire_format_with_values() {
        let it = item(true);
        assert_eq!(serde_json::to_string(&it).unwrap(), ITEM_FULL);
        assert_eq!(serde_json::from_str::<MediaItem>(ITEM_FULL).unwrap(), it);
    }

    #[test]
    fn media_item_from_api_2_0_server_has_no_file_hash() {
        let old = ITEM_EMPTY.replace(r#""file_hash":null,"#, "");
        assert!(!old.contains("file_hash"));
        assert_eq!(
            serde_json::from_str::<MediaItem>(&old).unwrap(),
            item(false)
        );
    }

    #[test]
    fn media_kind_wire_names() {
        let names: Vec<String> = [
            MediaKind::Image,
            MediaKind::Video,
            MediaKind::Audio,
            MediaKind::Other,
        ]
        .iter()
        .map(|k| serde_json::to_string(k).unwrap())
        .collect();
        assert_eq!(
            names,
            [r#""image""#, r#""video""#, r#""audio""#, r#""other""#]
        );
    }

    #[test]
    fn library_info_wire_format() {
        let info = LibraryInfo {
            library_id: "lib-1".into(),
            owner: "user-1".into(),
            epoch: "e1".into(),
            item_count: 42,
            scanning: true,
        };
        let json = r#"{"library_id":"lib-1","owner":"user-1","epoch":"e1","item_count":42,"scanning":true}"#;
        assert_eq!(serde_json::to_string(&info).unwrap(), json);
        assert_eq!(serde_json::from_str::<LibraryInfo>(json).unwrap(), info);
    }

    #[test]
    fn folder_listing_wire_format() {
        let empty = FolderListing {
            folders: vec![],
            items: vec![],
            next_cursor: None,
        };
        let json = r#"{"folders":[],"items":[],"next_cursor":null}"#;
        assert_eq!(serde_json::to_string(&empty).unwrap(), json);
        assert_eq!(serde_json::from_str::<FolderListing>(json).unwrap(), empty);

        let page = FolderListing {
            folders: vec!["2024".into()],
            items: vec![item(false)],
            next_cursor: Some("c1".into()),
        };
        let json = format!(r#"{{"folders":["2024"],"items":[{ITEM_EMPTY}],"next_cursor":"c1"}}"#);
        assert_eq!(serde_json::to_string(&page).unwrap(), json);
        assert_eq!(serde_json::from_str::<FolderListing>(&json).unwrap(), page);
    }

    #[test]
    fn item_id_is_transparent() {
        assert_eq!(
            serde_json::to_string(&ItemId("u".into())).unwrap(),
            r#""u""#
        );
    }
}
