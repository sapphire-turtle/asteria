use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use asteria_framework::{LibraryId, UserId, fsutil::atomic_write};
use serde::{Deserialize, Serialize};

use crate::error::{Result, ServerError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEntry {
    pub user: UserId,
    /// `blake3:<hex>` of the token; the token itself is never stored.
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub library: PathBuf,
    pub listen: SocketAddr,
    #[serde(default)]
    pub token: Vec<TokenEntry>,
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<ServerConfig> {
        let text = std::fs::read_to_string(path)?;
        let cfg: ServerConfig = toml::from_str(&text)
            .map_err(|e| ServerError::Config(format!("{}: {e}", path.display())))?;
        for entry in &cfg.token {
            if !crate::auth::is_valid_hash(&entry.hash) {
                return Err(ServerError::Config(format!(
                    "token entry for user {}: invalid hash",
                    entry.user
                )));
            }
        }
        Ok(cfg)
    }

    /// Writes atomically; on unix the file is 0600 (new file and overwrite alike).
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).map_err(|e| ServerError::Config(e.to_string()))?;
        // Pre-creating the file at 0600 means that if the write below fails, an empty
        // (zero-byte) file can remain; that is intentional, since it leaks nothing and
        // keeps the content from ever being written with looser permissions.
        #[cfg(unix)]
        restrict_permissions(path)?;
        atomic_write(path, text.as_bytes())?;
        Ok(())
    }
}

/// Makes sure `path` exists with mode 0600 so `atomic_write` (which keeps the target's
/// permission bits) never exposes the new content with looser permissions.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// `<cache_dir>/<app>/<library id>/`; falls back to `$HOME/.cache` when the platform has no cache dir.
pub fn cache_dir_for(app: &str, library: &LibraryId) -> Result<PathBuf> {
    let base = corundum_framework::platform::dirs::cache_dir()
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .ok_or_else(|| ServerError::Cache("cannot determine a cache directory".into()))?;
    Ok(base.join(app).join(library.0.hyphenated().to_string()))
}

/// `<data_dir>/<app>/<library id>/`, where this instance keeps its own state (journal, upload
/// sessions) apart from the shared workspace; falls back to `$HOME/.local/share` when the
/// platform has no data dir.
///
/// On Windows this is the local `%LOCALAPPDATA%` (corundum 0.4.0 and later), so per-host state
/// and upload sessions do not roam; `cli::main` moves an earlier version's Roaming directory
/// there before anything opens it.
pub fn data_dir_for(app: &str, library: &LibraryId) -> Result<PathBuf> {
    let base = corundum_framework::platform::dirs::data_dir()
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .ok_or_else(|| ServerError::Config("cannot determine a data directory".into()))?;
    Ok(base.join(app).join(library.0.hyphenated().to_string()))
}
