//! Upload wire types (API 2.2): dedupe check, chunked sessions, commit.

use serde::{Deserialize, Serialize};

/// Size of every chunk but the last, in bytes (8 MiB).
pub const UPLOAD_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// Largest file the server accepts, in bytes (20 GiB).
pub const UPLOAD_MAX_FILE: u64 = 20 * 1024 * 1024 * 1024;

/// A hash the client already knows, and which hash it is (`scope`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HashRef {
    /// `blake3:<hex>`.
    pub hash: String,
    pub scope: String,
}

/// Parameters of `upload.check`. At most 1000 hashes; more is `INVALID_PARAMS`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadCheckParams {
    pub hashes: Vec<HashRef>,
}

/// A hash the library already holds, and where.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExistingMedia {
    pub hash: String,
    pub scope: String,
    /// Library-relative, `/`-separated.
    pub paths: Vec<String>,
}

/// Result of `upload.check`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadCheckResult {
    pub existing: Vec<ExistingMedia>,
}

/// Parameters of `upload.begin`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadBeginParams {
    pub size: u64,
    /// The client's claim; the server hashes the file itself at commit.
    pub file_hash: String,
}

/// Result of `upload.begin`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadBegin {
    pub session: String,
    pub chunk_size: u64,
}

/// Parameters of `upload.status` and `upload.abort`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadSessionParams {
    pub session: String,
}

/// Result of `upload.status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadStatus {
    pub size: u64,
    /// Indexes of the chunks not yet received.
    pub missing: Vec<u64>,
}

/// One session to place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadFile {
    pub session: String,
    /// Relative to `Inbox/`, `/`-separated.
    pub path: String,
}

/// Parameters of `upload.commit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadCommitParams {
    pub files: Vec<UploadFile>,
    pub allow_duplicate: bool,
}

/// Where a session's file ended up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacedFile {
    pub session: String,
    /// Library-relative (`Inbox/...`).
    pub path: String,
}

/// Something the server did not do as asked, without failing the commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UploadWarning {
    SidecarDropped { session: String, reason: String },
}

/// Result of `upload.commit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum UploadCommitResult {
    Placed {
        files: Vec<PlacedFile>,
        warnings: Vec<UploadWarning>,
    },
    /// Nothing was placed: the library already holds these paths.
    Duplicate { existing: Vec<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_result_wire() {
        let dup = UploadCommitResult::Duplicate {
            existing: vec!["2025/a.jpg".into()],
        };
        let json = r#"{"outcome":"duplicate","existing":["2025/a.jpg"]}"#;
        assert_eq!(serde_json::to_string(&dup).unwrap(), json);
        assert_eq!(
            serde_json::from_str::<UploadCommitResult>(json).unwrap(),
            dup
        );

        let placed = UploadCommitResult::Placed {
            files: vec![PlacedFile {
                session: "s".into(),
                path: "Inbox/a.jpg".into(),
            }],
            warnings: vec![UploadWarning::SidecarDropped {
                session: "t".into(),
                reason: "bad xml".into(),
            }],
        };
        let json = concat!(
            r#"{"outcome":"placed","files":[{"session":"s","path":"Inbox/a.jpg"}],"#,
            r#""warnings":[{"kind":"sidecar_dropped","session":"t","reason":"bad xml"}]}"#
        );
        assert_eq!(serde_json::to_string(&placed).unwrap(), json);
        assert_eq!(
            serde_json::from_str::<UploadCommitResult>(json).unwrap(),
            placed
        );
    }

    #[test]
    fn params_and_results_wire() {
        let p = UploadCheckParams {
            hashes: vec![HashRef {
                hash: "blake3:ab".into(),
                scope: "content".into(),
            }],
        };
        let json = r#"{"hashes":[{"hash":"blake3:ab","scope":"content"}]}"#;
        assert_eq!(serde_json::to_string(&p).unwrap(), json);
        assert_eq!(serde_json::from_str::<UploadCheckParams>(json).unwrap(), p);

        let r = UploadCheckResult {
            existing: vec![ExistingMedia {
                hash: "blake3:ab".into(),
                scope: "content".into(),
                paths: vec!["a.jpg".into()],
            }],
        };
        let json = r#"{"existing":[{"hash":"blake3:ab","scope":"content","paths":["a.jpg"]}]}"#;
        assert_eq!(serde_json::to_string(&r).unwrap(), json);
        assert_eq!(serde_json::from_str::<UploadCheckResult>(json).unwrap(), r);

        let b = UploadBegin {
            session: "s".into(),
            chunk_size: UPLOAD_CHUNK_SIZE,
        };
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"session":"s","chunk_size":8388608}"#
        );
        let st = UploadStatus {
            size: 10,
            missing: vec![0, 2],
        };
        assert_eq!(
            serde_json::to_string(&st).unwrap(),
            r#"{"size":10,"missing":[0,2]}"#
        );
        let c = UploadCommitParams {
            files: vec![UploadFile {
                session: "s".into(),
                path: "a/b.jpg".into(),
            }],
            allow_duplicate: false,
        };
        let json = r#"{"files":[{"session":"s","path":"a/b.jpg"}],"allow_duplicate":false}"#;
        assert_eq!(serde_json::to_string(&c).unwrap(), json);
        assert_eq!(serde_json::from_str::<UploadCommitParams>(json).unwrap(), c);
    }

    #[test]
    fn limits_and_codes() {
        assert_eq!(UPLOAD_CHUNK_SIZE, 8 * 1024 * 1024);
        assert_eq!(UPLOAD_MAX_FILE, 20 * 1024 * 1024 * 1024);
        assert_eq!(crate::codes::QUOTA, -32006);
        assert_eq!(crate::codes::UPLOAD_INCOMPLETE, -32007);
        assert_eq!(crate::codes::HASH_MISMATCH, -32008);
        assert_eq!(crate::UPLOAD_CHECK, "upload.check");
    }
}
