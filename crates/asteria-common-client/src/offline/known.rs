use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use asteria_common_api::LibraryInfo;

use crate::media_cache::write_atomic;
use crate::profiles::ProfileId;

/// The libraries the profiles last connected to, kept so a profile can be opened offline.
/// `<data>/offline/libraries.json`, keyed by profile id.
#[derive(Debug)]
pub struct KnownLibraries {
    path: PathBuf,
}

impl KnownLibraries {
    /// Does no I/O.
    pub fn new(data_root: &Path) -> KnownLibraries {
        KnownLibraries {
            path: data_root.join("offline").join("libraries.json"),
        }
    }

    /// What `profile` last saw; `None` if never recorded or the file is unreadable.
    pub fn get(&self, profile: ProfileId) -> Option<LibraryInfo> {
        self.load().remove(&profile.to_string())
    }

    pub fn record(&self, profile: ProfileId, info: &LibraryInfo) -> io::Result<()> {
        let mut all = self.load();
        all.insert(profile.to_string(), info.clone());
        write_atomic(&self.path, &serde_json::to_vec(&all)?, true)
    }

    fn load(&self) -> HashMap<String, LibraryInfo> {
        fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, revision: u64) -> LibraryInfo {
        LibraryInfo {
            library_id: id.into(),
            owner: "me".into(),
            epoch: "e".into(),
            item_count: 3,
            scanning: false,
            revision,
        }
    }

    #[test]
    fn record_then_get() {
        let dir = tempfile::tempdir().unwrap();
        let known = KnownLibraries::new(dir.path());
        let (a, b) = (
            ProfileId(grain_id::GrainId::random()),
            ProfileId(grain_id::GrainId::random()),
        );
        assert_eq!(known.get(a), None);
        known.record(a, &info("l1", 1)).unwrap();
        known.record(b, &info("l2", 1)).unwrap();
        assert_eq!(known.get(a), Some(info("l1", 1)));
        assert_eq!(known.get(b), Some(info("l2", 1)));
        known.record(a, &info("l1", 2)).unwrap();
        assert_eq!(known.get(a), Some(info("l1", 2)));
        assert_eq!(known.get(b), Some(info("l2", 1)));
    }

    #[test]
    fn a_corrupt_file_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let known = KnownLibraries::new(dir.path());
        let a = ProfileId(grain_id::GrainId::random());
        std::fs::create_dir_all(dir.path().join("offline")).unwrap();
        std::fs::write(dir.path().join("offline/libraries.json"), "{oops").unwrap();
        assert_eq!(known.get(a), None);
        known.record(a, &info("l1", 1)).unwrap();
        assert_eq!(known.get(a), Some(info("l1", 1)));
    }
}
