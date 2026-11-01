//! An API key's profile is a user of the library, by name: the key acts as that user. Only
//! the owner can be chosen for now; registering other users comes later.

use std::path::Path;

use asteria_framework::fsutil::atomic_write;
use asteria_framework::{Library, UserId};
use corundum_framework::keys::{Profile, ProfileId};

use crate::error::{Result, ServerError};

/// The profiles a key may be added with: the library's users, the owner's labelled so.
pub fn profiles(library: &Library) -> Vec<Profile> {
    library
        .users()
        .user
        .iter()
        .filter_map(|user| {
            let id = ProfileId::new(&user.name).ok()?;
            let label = if user.id == library.owner() {
                format!("{} (owner)", user.name)
            } else {
                user.name.clone()
            };
            Some(Profile { id, label })
        })
        .collect()
}

/// The owner's profile: their user name.
pub fn owner_profile(library: &Library) -> Result<ProfileId> {
    let owner = library.owner();
    let user = library
        .users()
        .user
        .iter()
        .find(|u| u.id == owner)
        .ok_or_else(|| ServerError::Config(format!("the owner {owner} is not in users.toml")))?;
    ProfileId::new(&user.name)
        .map_err(|_| ServerError::Config(format!("the owner {owner} has an empty name")))
}

/// The user a key of `profile` acts as, if the library has one by that name.
pub fn user_of(library: &Library, profile: &ProfileId) -> Option<UserId> {
    library.user_by_name(profile.as_str()).map(|u| u.id)
}

/// Gives `profile` to the keys in `dir` that have none, as keys added before corundum 0.7
/// do (it skips them, refusing their tokens). Their `subject`, never used, is dropped. A file
/// that is not a key is left alone. How many keys were changed.
pub(crate) fn adopt_keys_without_profile(dir: &Path, profile: &ProfileId) -> Result<usize> {
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
        key.insert("profile".into(), profile.as_str().into());
        let text = toml::to_string(&key)
            .map_err(|e| ServerError::Config(format!("{}: {e}", path.display())))?;
        atomic_write(&path, text.as_bytes())?;
        adopted += 1;
    }
    if adopted > 0 {
        tracing::info!(dir = %dir.display(), "gave {adopted} API keys the owner's profile");
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corundum_framework::keys::KeyStore;

    fn pid(s: &str) -> ProfileId {
        ProfileId::new(s).unwrap()
    }

    /// A key added as corundum 0.6 wrote it: `subject` in place of `profile`.
    fn old_key(dir: &Path, name: &str) -> String {
        let mut keys = KeyStore::open(dir).unwrap();
        let (key, token) = keys.add(name, None, pid("placeholder")).unwrap();
        let file = dir.join(format!("{}.toml", key.id));
        let text = std::fs::read_to_string(&file).unwrap();
        let text = text.replace("profile = \"placeholder\"", "subject = \"me\"");
        assert!(!text.contains("profile"), "{text}");
        std::fs::write(&file, text).unwrap();
        token.expose().to_owned()
    }

    #[test]
    fn a_key_without_a_profile_becomes_the_owners() {
        let dir = tempfile::tempdir().unwrap();
        let token = old_key(dir.path(), "phone");
        assert!(KeyStore::open(dir.path()).unwrap().list().is_empty());

        assert_eq!(
            adopt_keys_without_profile(dir.path(), &pid("fluo10")).unwrap(),
            1
        );
        let keys = KeyStore::open(dir.path()).unwrap();
        let key = keys.authenticate(&token).expect("accepted again");
        assert_eq!(key.name, "phone");
        assert_eq!(key.profile, pid("fluo10"));
        // Once is enough.
        assert_eq!(
            adopt_keys_without_profile(dir.path(), &pid("other")).unwrap(),
            0
        );
    }

    #[test]
    fn other_files_and_keys_with_a_profile_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut keys = KeyStore::open(dir.path()).unwrap();
        let (key, _) = keys.add("pad", None, pid("reader")).unwrap();
        let key_file = dir.path().join(format!("{}.toml", key.id));
        let before = std::fs::read_to_string(&key_file).unwrap();
        std::fs::write(dir.path().join("notes.toml"), "x = 1\n").unwrap();
        std::fs::write(dir.path().join("broken.toml"), "not toml [").unwrap();

        assert_eq!(
            adopt_keys_without_profile(dir.path(), &pid("fluo10")).unwrap(),
            0
        );
        assert_eq!(std::fs::read_to_string(&key_file).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.toml")).unwrap(),
            "x = 1\n"
        );
    }

    #[test]
    fn a_missing_dir_has_nothing_to_adopt() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no");
        assert_eq!(
            adopt_keys_without_profile(&missing, &pid("fluo10")).unwrap(),
            0
        );
    }

    #[test]
    fn the_profiles_are_the_users_and_the_owner_is_labelled() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::init(dir.path(), "fluo10").unwrap();
        assert_eq!(
            profiles(&lib),
            vec![Profile {
                id: pid("fluo10"),
                label: "fluo10 (owner)".into()
            }]
        );
        assert_eq!(owner_profile(&lib).unwrap(), pid("fluo10"));
        assert_eq!(user_of(&lib, &pid("fluo10")), Some(lib.owner()));
        assert_eq!(user_of(&lib, &pid("owner")), None);
    }
}
