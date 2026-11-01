//! The one profile an API key acts as: the library's owner. corundum lets an app offer
//! several (a read-only one, say); asteria offers only this one for now.

use std::path::Path;

use asteria_framework::fsutil::atomic_write;
use corundum_framework::keys::{Profile, ProfileId};

use crate::error::{Result, ServerError};

/// The id every key is given.
pub const OWNER: &str = "owner";

/// [`OWNER`] as a profile id.
pub fn owner() -> ProfileId {
    ProfileId::new(OWNER).expect("not empty")
}

/// The profiles a key may be added with.
pub fn profiles() -> Vec<Profile> {
    vec![Profile {
        id: owner(),
        label: "Owner".into(),
    }]
}

/// Gives [`OWNER`] to the keys in `dir` that have no profile, as keys added before
/// corundum 0.7 do (it skips them, refusing their tokens). Their `subject`, never used, is
/// dropped. A file that is not a key is left alone. How many keys were changed.
pub(crate) fn adopt_keys_without_profile(dir: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut adopted = 0;
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // As the store reads: hidden files are a write in progress, or not a key.
        if name.starts_with('.') || !name.ends_with(".toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut key) = text.parse::<toml::Table>() else {
            continue;
        };
        let is_key = key.contains_key("id") && key.contains_key("hash");
        let has_profile = key
            .get("profile")
            .and_then(|p| p.as_str())
            .is_some_and(|p| !p.is_empty());
        if !is_key || has_profile {
            continue;
        }
        key.remove("subject");
        key.insert("profile".into(), OWNER.into());
        let text = toml::to_string(&key)
            .map_err(|e| ServerError::Config(format!("{}: {e}", path.display())))?;
        atomic_write(&path, text.as_bytes())?;
        adopted += 1;
    }
    if adopted > 0 {
        tracing::info!(dir = %dir.display(), "gave {adopted} API keys the owner profile");
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corundum_framework::keys::KeyStore;

    /// A key added as corundum 0.6 wrote it: `subject` in place of `profile`.
    fn old_key(dir: &Path, name: &str) -> String {
        let mut keys = KeyStore::open(dir).unwrap();
        let (key, token) = keys.add(name, None, owner()).unwrap();
        let file = dir.join(format!("{}.toml", key.id));
        let text = std::fs::read_to_string(&file).unwrap();
        let text = text.replace("profile = \"owner\"", "subject = \"me\"");
        assert!(!text.contains("profile"), "{text}");
        std::fs::write(&file, text).unwrap();
        token.expose().to_owned()
    }

    #[test]
    fn a_key_without_a_profile_becomes_the_owners() {
        let dir = tempfile::tempdir().unwrap();
        let token = old_key(dir.path(), "phone");
        assert!(KeyStore::open(dir.path()).unwrap().list().is_empty());

        assert_eq!(adopt_keys_without_profile(dir.path()).unwrap(), 1);
        let keys = KeyStore::open(dir.path()).unwrap();
        let key = keys.authenticate(&token).expect("accepted again");
        assert_eq!(key.name, "phone");
        assert_eq!(key.profile, owner());
        // Once is enough.
        assert_eq!(adopt_keys_without_profile(dir.path()).unwrap(), 0);
    }

    #[test]
    fn other_files_and_keys_with_a_profile_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut keys = KeyStore::open(dir.path()).unwrap();
        let (key, _) = keys
            .add("pad", None, ProfileId::new("reader").unwrap())
            .unwrap();
        let key_file = dir.path().join(format!("{}.toml", key.id));
        let before = std::fs::read_to_string(&key_file).unwrap();
        std::fs::write(dir.path().join("notes.toml"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("broken.toml"), "not toml [").unwrap();

        assert_eq!(adopt_keys_without_profile(dir.path()).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&key_file).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.toml")).unwrap(),
            "x = 1\n"
        );
    }

    #[test]
    fn a_missing_dir_has_nothing_to_adopt() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            adopt_keys_without_profile(&dir.path().join("no")).unwrap(),
            0
        );
    }
}
