use std::fs;
use std::path::{Path, PathBuf};

use grain_id::GrainId;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsutil::atomic_write;
use crate::ids::{LibraryId, UserId};
use crate::paths::META_DIR;

const FORMAT: u32 = 1;
const LIBRARY_FILE: &str = "library.toml";
const USERS_FILE: &str = "users.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryConfig {
    pub format: u32,
    pub id: LibraryId,
    pub owner: UserId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Users {
    #[serde(default)]
    pub user: Vec<User>,
}

#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
    config: LibraryConfig,
    users: Users,
}

impl Library {
    pub fn init(root: &Path, owner_name: &str) -> Result<Library> {
        let meta = root.join(META_DIR);
        // library.toml is written last, so its presence means a complete init; a directory left
        // behind by a failed init can be initialized again.
        if meta.join(LIBRARY_FILE).exists() {
            return Err(Error::AlreadyInitialized(root.to_path_buf()));
        }
        fs::create_dir_all(&meta)?;
        let owner = UserId(GrainId::random());
        let config = LibraryConfig {
            format: FORMAT,
            id: LibraryId(uuid::Uuid::new_v4()),
            owner,
        };
        let users = Users {
            user: vec![User {
                id: owner,
                name: owner_name.to_owned(),
            }],
        };
        atomic_write(
            &meta.join(USERS_FILE),
            toml::to_string(&users)
                .map_err(|e| Error::Toml(e.to_string()))?
                .as_bytes(),
        )?;
        atomic_write(
            &meta.join(LIBRARY_FILE),
            toml::to_string(&config)
                .map_err(|e| Error::Toml(e.to_string()))?
                .as_bytes(),
        )?;
        Ok(Library {
            root: root.to_path_buf(),
            config,
            users,
        })
    }

    pub fn open(root: &Path) -> Result<Library> {
        let meta = root.join(META_DIR);
        let config_text = match fs::read_to_string(meta.join(LIBRARY_FILE)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotALibrary(root.to_path_buf()));
            }
            Err(e) => return Err(e.into()),
        };
        let config: LibraryConfig =
            toml::from_str(&config_text).map_err(|e| Error::Toml(e.to_string()))?;
        if config.format != FORMAT {
            return Err(Error::UnsupportedFormat(config.format));
        }
        let users_path = meta.join(USERS_FILE);
        let users_text = match fs::read_to_string(&users_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::MissingFile(users_path));
            }
            Err(e) => return Err(e.into()),
        };
        let users: Users = toml::from_str(&users_text).map_err(|e| Error::Toml(e.to_string()))?;
        Ok(Library {
            root: root.to_path_buf(),
            config,
            users,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &LibraryConfig {
        &self.config
    }

    pub fn users(&self) -> &Users {
        &self.users
    }

    pub fn owner(&self) -> UserId {
        self.config.owner
    }
}
