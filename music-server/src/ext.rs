//! The music app's plug-in for the shared server.

use std::path::Path;

use asteria_framework::MediaKind;
use asteria_framework_server::AppExtension;

use crate::tags;

/// Music shows audio; the tags are read during the scan.
pub struct MusicExtension;

impl MusicExtension {
    pub fn new() -> Self {
        Self
    }
}

impl AppExtension for MusicExtension {
    fn accepts(&self, kind: MediaKind) -> bool {
        kind == MediaKind::Audio
    }

    fn extract(&self, _rel: &str, abs: &Path) -> Option<Vec<u8>> {
        serde_json::to_vec(&tags::read(abs)).ok()
    }
}
