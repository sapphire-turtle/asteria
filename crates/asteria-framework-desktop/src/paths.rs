//! Where the local server's files are: the default places, or a temporary directory's in
//! the tests.

use std::path::PathBuf;

use asteria_framework::LibraryId;
use asteria_framework_server::error::Result;
use asteria_framework_server::keys::KeyStore;
use asteria_framework_server::{default_config_path, open_keys};

/// The local server's config file and the library's API keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalPaths {
    /// The config file, `server.toml`.
    pub config: PathBuf,
    /// The directory of the library's keys; `None` for the library's own in the app's data
    /// directory, as the server reads them.
    pub keys: Option<PathBuf>,
}

impl LocalPaths {
    /// The places the server itself uses: the default config path, and the keys in the data
    /// directory.
    pub fn default_for(app: &str) -> Result<LocalPaths> {
        Ok(LocalPaths {
            config: default_config_path(app)?,
            keys: None,
        })
    }

    /// The store of `library`'s keys (created if absent); new tokens start with `<app>-`.
    pub(crate) fn open_keys(&self, app: &str, library: &LibraryId) -> Result<KeyStore> {
        match &self.keys {
            None => open_keys(app, library),
            Some(dir) => Ok(KeyStore::open(dir)?.token_prefix(&format!("{app}-"))),
        }
    }
}
