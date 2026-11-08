use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use asteria_common_api::ItemId;
use serde::{Deserialize, Serialize};

use super::DOC_SCHEMA;
use crate::media_cache::{TMP_SUFFIX, hash_hex, safe_component, write_atomic};

/// A temporary file this old is the leftover of a crashed write, not one in progress.
const STALE_TMP: Duration = Duration::from_secs(24 * 3600);

/// What the user asked to keep offline: an album, a playlist, and so on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PinTarget {
    pub kind: String,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PinStatus {
    /// Not copied yet.
    Pending,
    Saved,
    /// The server no longer has the target.
    Gone,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub target: PinTarget,
    pub added_unix: u64,
    pub status: PinStatus,
}

/// A copy of one server answer, keyed by what was asked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Doc {
    pub kind: String,
    pub id: String,
    pub value: serde_json::Value,
}

/// One media file a pin needs: the item, its content hash, the preset and the size the server
/// estimated.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MediaKey {
    pub item: ItemId,
    pub file_hash: String,
    pub preset: String,
    pub estimate: u64,
}

/// Where the last sync stopped: the library's epoch and revision, and a digest of the pins it
/// covered.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncMark {
    pub epoch: String,
    pub revision: u64,
    pub pins_digest: String,
}

/// The on-disk form of a [`Doc`].
#[derive(Serialize, Deserialize)]
struct DocFile {
    schema: u32,
    kind: String,
    id: String,
    value: serde_json::Value,
}

/// The offline copy of one library, at `<data>/offline/<library id>`: `pins.json`, `state.json`,
/// `docs/<kind>/<hex of blake3(id)>.json` and `media/<preset>/<hex[..2]>/<hex>.<ext>`.
///
/// Every method does blocking disk I/O: call them from a worker thread, never the UI thread.
#[derive(Debug)]
pub struct OfflineStore {
    root: PathBuf,
}

impl OfflineStore {
    /// The store of `library_id` under `data_root`. Does no I/O. `None` if the id is not a safe
    /// path component (see [`safe_component`]).
    pub fn open(data_root: &Path, library_id: &str) -> Option<OfflineStore> {
        safe_component(library_id).then(|| OfflineStore {
            root: data_root.join("offline").join(library_id),
        })
    }

    /// The stored file for `(file_hash, preset)`, if there is one.
    pub fn media(&self, file_hash: &str, preset: &str) -> Option<PathBuf> {
        let path = self.media_path(file_hash, preset).ok()?;
        fs::metadata(&path).ok()?.is_file().then_some(path)
    }

    pub fn put_media(&self, file_hash: &str, preset: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        let path = self.media_path(file_hash, preset)?;
        write_atomic(&path, bytes, false)?;
        Ok(path)
    }

    /// The total size of the stored media files.
    pub fn media_bytes(&self) -> u64 {
        media_files(&self.root.join("media")).map(|f| f.len).sum()
    }

    /// Every stored answer copy. A file that is not JSON or has another `schema` is deleted.
    pub fn read_docs(&self) -> Vec<Doc> {
        let mut docs = Vec::new();
        for kind_dir in subdirs(&self.root.join("docs")) {
            for entry in fs::read_dir(&kind_dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let parsed = fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<DocFile>(&b).ok())
                    .filter(|d| d.schema == DOC_SCHEMA);
                match parsed {
                    Some(d) => docs.push(Doc {
                        kind: d.kind,
                        id: d.id,
                        value: d.value,
                    }),
                    None => {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }
        docs
    }

    pub fn write_doc(&self, doc: &Doc) -> io::Result<()> {
        let path = self.doc_path(&doc.kind, &doc.id)?;
        let file = DocFile {
            schema: DOC_SCHEMA,
            kind: doc.kind.clone(),
            id: doc.id.clone(),
            value: doc.value.clone(),
        };
        write_atomic(&path, &serde_json::to_vec(&file)?, false)
    }

    /// Deletes a stored answer copy; none stored is fine.
    pub fn remove_doc(&self, kind: &str, id: &str) -> io::Result<()> {
        match fs::remove_file(self.doc_path(kind, id)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub fn load_pins(&self) -> Vec<Pin> {
        self.load_json("pins.json").unwrap_or_default()
    }

    pub fn save_pins(&self, pins: &[Pin]) -> io::Result<()> {
        self.save_json("pins.json", &pins)
    }

    pub fn load_mark(&self) -> SyncMark {
        self.load_json("state.json").unwrap_or_default()
    }

    pub fn save_mark(&self, mark: &SyncMark) -> io::Result<()> {
        self.save_json("state.json", mark)
    }

    /// Deletes media files not in `keep` ((file_hash, preset)); returns the bytes freed.
    pub fn collect_garbage(&self, keep: &HashSet<(String, String)>) -> io::Result<u64> {
        let mut freed = 0;
        for f in media_files(&self.root.join("media")).collect::<Vec<_>>() {
            if !keep.contains(&(f.hash.clone(), f.preset.clone())) {
                fs::remove_file(&f.path)?;
                freed += f.len;
            }
        }
        Ok(freed)
    }

    /// Deletes the leftovers of writes that crashed (temporary files over a day old).
    pub fn sweep_stale_tmp(&self) {
        for entry in walkdir::WalkDir::new(&self.root).into_iter().flatten() {
            let stale = entry.file_type().is_file()
                && entry.file_name().to_string_lossy().ends_with(TMP_SUFFIX)
                && entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age > STALE_TMP);
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Deletes the whole offline copy of the library.
    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.root) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn load_json<T: serde::de::DeserializeOwned>(&self, name: &str) -> Option<T> {
        serde_json::from_slice(&fs::read(self.root.join(name)).ok()?).ok()
    }

    /// Pins and the sync mark cannot be rebuilt from the server, so they are synced to disk.
    fn save_json<T: Serialize + ?Sized>(&self, name: &str, value: &T) -> io::Result<()> {
        write_atomic(&self.root.join(name), &serde_json::to_vec(value)?, true)
    }

    fn media_path(&self, file_hash: &str, preset: &str) -> io::Result<PathBuf> {
        let invalid = |what| io::Error::new(io::ErrorKind::InvalidInput, what);
        if !safe_component(preset) {
            return Err(invalid("unsafe media preset"));
        }
        let hex = hash_hex(file_hash)
            .ok_or_else(|| invalid("file hash is not blake3:<64 lowercase hex>"))?;
        Ok(self
            .root
            .join("media")
            .join(preset)
            .join(&hex[..2])
            .join(format!("{hex}.{}", extension(preset))))
    }

    /// The id is hashed, so whatever it contains never reaches the file system.
    fn doc_path(&self, kind: &str, id: &str) -> io::Result<PathBuf> {
        if !safe_component(kind) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe doc kind",
            ));
        }
        Ok(self
            .root
            .join("docs")
            .join(kind)
            .join(format!("{}.json", blake3::hash(id.as_bytes()).to_hex())))
    }
}

fn extension(preset: &str) -> &'static str {
    if preset == "offline-audio" {
        "ogg"
    } else {
        "webp"
    }
}

/// A stored media file, as found on disk.
struct MediaFile {
    path: PathBuf,
    preset: String,
    /// `blake3:<hex>`.
    hash: String,
    len: u64,
}

/// The finished files under `media/` (temporary files are skipped).
fn media_files(dir: &Path) -> impl Iterator<Item = MediaFile> {
    walkdir::WalkDir::new(dir)
        .min_depth(3)
        .max_depth(3)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy();
            let stem = name.strip_suffix(".ogg").or(name.strip_suffix(".webp"))?;
            let preset = e.path().parent()?.parent()?.file_name()?.to_string_lossy();
            Some(MediaFile {
                preset: preset.into_owned(),
                hash: format!("blake3:{stem}"),
                len: e.metadata().ok()?.len(),
                path: e.path().to_path_buf(),
            })
        })
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    const HASH_A: &str = "blake3:abababababababababababababababababababababababababababababababab";
    const HASH_B: &str = "blake3:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
    const HASH_C: &str = "blake3:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

    fn store(dir: &tempfile::TempDir) -> OfflineStore {
        OfflineStore::open(dir.path(), "lib1").unwrap()
    }

    fn doc(kind: &str, id: &str) -> Doc {
        Doc {
            kind: kind.into(),
            id: id.into(),
            value: serde_json::json!({"n": 1}),
        }
    }

    #[test]
    fn media_round_trips_and_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let path = s.put_media(HASH_A, "offline-audio", b"ogg").unwrap();
        assert!(path.ends_with(format!("media/offline-audio/ab/{}.ogg", &HASH_A[7..])));
        assert_eq!(s.media(HASH_A, "offline-audio"), Some(path));
        assert_eq!(s.media(HASH_B, "offline-audio"), None);
        assert_eq!(s.media_bytes(), 3);
    }

    #[test]
    fn unsafe_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(OfflineStore::open(dir.path(), "..").is_none());
        let s = store(&dir);
        let kind = |e: io::Result<_>| e.map(|_: PathBuf| ()).unwrap_err().kind();
        assert_eq!(
            kind(s.put_media("md5:x", "thumb", b"x")),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            kind(s.put_media(HASH_A, "../x", b"x")),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            s.write_doc(&doc("a/b", "1")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn docs_round_trip_and_bad_files_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.write_doc(&doc("album", "a")).unwrap();
        s.write_doc(&doc("album", "b")).unwrap();
        let folder = dir.path().join("offline/lib1/docs/album");
        let wrong = folder.join("wrong.json");
        let junk = folder.join("junk.json");
        fs::write(&wrong, r#"{"schema":99,"kind":"album","id":"c","value":1}"#).unwrap();
        fs::write(&junk, "not json").unwrap();
        let mut docs = s.read_docs();
        docs.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(docs, vec![doc("album", "a"), doc("album", "b")]);
        assert!(!wrong.exists() && !junk.exists());
        s.remove_doc("album", "a").unwrap();
        s.remove_doc("album", "a").unwrap();
        assert_eq!(s.read_docs(), vec![doc("album", "b")]);
    }

    #[test]
    fn doc_ids_never_become_paths() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.write_doc(&doc("album", "../../etc")).unwrap();
        let kind_dir = dir.path().join("offline/lib1/docs/album");
        assert_eq!(fs::read_dir(&kind_dir).unwrap().count(), 1);
        let count = walkdir::WalkDir::new(dir.path())
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .count();
        assert_eq!(count, 1);
        assert_eq!(s.read_docs()[0].id, "../../etc");
    }

    #[test]
    fn garbage_collection_keeps_what_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.put_media(HASH_A, "offline-audio", b"aaaa").unwrap();
        s.put_media(HASH_B, "offline-audio", b"bb").unwrap();
        s.put_media(HASH_C, "offline-photo", b"c").unwrap();
        let keep: HashSet<_> = [
            (HASH_A.to_string(), "offline-audio".to_string()),
            (HASH_C.to_string(), "offline-photo".to_string()),
        ]
        .into();
        assert_eq!(s.collect_garbage(&keep).unwrap(), 2);
        assert!(s.media(HASH_A, "offline-audio").is_some());
        assert!(s.media(HASH_B, "offline-audio").is_none());
        assert!(s.media(HASH_C, "offline-photo").is_some());
        assert_eq!(s.media_bytes(), 5);
    }

    #[test]
    fn stale_tmp_is_swept_fresh_tmp_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.write_doc(&doc("album", "a")).unwrap();
        let folder = dir.path().join("offline/lib1/docs/album");
        let old = folder.join(".x.1-0.asteria-tmp");
        let fresh = folder.join(".x.1-1.asteria-tmp");
        let f = File::create(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(25 * 3600))
            .unwrap();
        File::create(&fresh).unwrap();
        s.sweep_stale_tmp();
        assert!(!old.exists());
        assert!(fresh.exists());
        assert_eq!(s.read_docs().len(), 1);
    }

    #[test]
    fn pins_and_mark_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let pins = vec![Pin {
            target: PinTarget {
                kind: "album".into(),
                id: "a".into(),
                name: "Trip".into(),
            },
            added_unix: 5,
            status: PinStatus::Pending,
        }];
        s.save_pins(&pins).unwrap();
        assert_eq!(s.load_pins(), pins);
        let mark = SyncMark {
            epoch: "e".into(),
            revision: 7,
            pins_digest: "d".into(),
        };
        s.save_mark(&mark).unwrap();
        assert_eq!(s.load_mark(), mark);
        s.clear().unwrap();
        assert!(s.load_pins().is_empty());
        s.clear().unwrap();
    }

    #[test]
    fn a_missing_store_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        assert!(s.load_pins().is_empty());
        assert_eq!(s.load_mark(), SyncMark::default());
        assert!(s.read_docs().is_empty());
        assert_eq!(s.media_bytes(), 0);
        assert_eq!(s.media(HASH_A, "offline-audio"), None);
    }
}
