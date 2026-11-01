use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use asteria_framework::{Library, LibraryId, UserId, fsutil::atomic_write};
use chrono::Utc;
use corundum_framework::keys::{Key, KeyHash, KeyId, KeyStore};
use serde::{Deserialize, Serialize};

use crate::error::{Result, ServerError};

/// The server's config file: which library it serves and where it listens. Who may connect
/// is not here: that is the API keys in [`keys_dir_for`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub library: PathBuf,
    pub listen: SocketAddr,
}

/// A `[[token]]` entry of an older config: a token's hash and the user it stood for. Only
/// read, to be imported as a key by [`load_and_migrate`].
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LegacyToken {
    pub(crate) user: UserId,
    /// `blake3:<hex>` of the token.
    pub(crate) hash: String,
}

/// The config file as read: the current fields, and any `[[token]]` of an older version.
#[derive(Deserialize)]
struct RawConfig {
    library: PathBuf,
    listen: SocketAddr,
    #[serde(default)]
    token: Vec<LegacyToken>,
}

fn read_raw(path: &Path) -> Result<RawConfig> {
    let text = std::fs::read_to_string(path)?;
    toml::from_str(&text).map_err(|e| ServerError::Config(format!("{}: {e}", path.display())))
}

impl ServerConfig {
    /// Reads the config at `path`. One that still has `[[token]]` entries is refused: open it
    /// with [`load_and_migrate`], which makes them keys.
    pub fn load(path: &Path) -> Result<ServerConfig> {
        let raw = read_raw(path)?;
        if !raw.token.is_empty() {
            return Err(ServerError::Config(format!(
                "{} has [[token]] entries of an older version; they are imported as API keys \
                 when a server command opens it",
                path.display()
            )));
        }
        Ok(ServerConfig {
            library: raw.library,
            listen: raw.listen,
        })
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

/// Reads the config at `path`, as every server command does. An older config's `[[token]]`
/// entries are imported as API keys of the library (`token-1`, `token-2`…, their hashes as
/// they are, so the tokens keep working), and the config is then saved without them. If any
/// entry cannot be imported, nothing is saved and the error is returned.
pub fn load_and_migrate(app: &str, path: &Path) -> Result<ServerConfig> {
    let raw = read_raw(path)?;
    let cfg = ServerConfig {
        library: raw.library,
        listen: raw.listen,
    };
    if raw.token.is_empty() {
        return Ok(cfg);
    }
    // Every hash is checked before anything is imported. Neither the hash nor the token
    // appears in the error.
    let hashes = raw
        .token
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            entry.hash.parse::<KeyHash>().map_err(|_| {
                ServerError::Config(format!(
                    "{}: [[token]] {} (user {}): invalid hash",
                    path.display(),
                    i + 1,
                    entry.user
                ))
            })
        })
        .collect::<Result<Vec<KeyHash>>>()?;
    let library = Library::open(&cfg.library.canonicalize()?)?;
    let mut keys = open_keys(app, &library.config().id)?;
    for (i, hash) in hashes.into_iter().enumerate() {
        // Imported by an earlier run whose save failed: not again.
        if keys.list().iter().any(|k| k.hash == hash) {
            continue;
        }
        keys.import(Key {
            id: KeyId::generate(),
            name: format!("token-{}", i + 1),
            description: None,
            subject: None,
            hash,
            created_at: Utc::now(),
            rotated_at: None,
            retired_at: None,
        })?;
    }
    cfg.save(path)?;
    tracing::info!(
        imported = raw.token.len(),
        config = %path.display(),
        "the config's tokens are API keys now"
    );
    Ok(cfg)
}

/// `<config_dir>/<app>/server.toml`: the config file when none is named.
pub fn default_config_path(app: &str) -> Result<PathBuf> {
    let dirs = corundum_framework::platform::dirs::AppDirs::new(app)
        .ok_or_else(|| ServerError::Config("cannot determine a config directory".into()))?;
    Ok(dirs.config().join("server.toml"))
}

/// `<data_dir>/<app>/<library id>/keys/`: the library's API keys on this host.
pub fn keys_dir_for(app: &str, library: &LibraryId) -> Result<PathBuf> {
    Ok(data_dir_for(app, library)?.join("keys"))
}

/// The store of the library's API keys (created if absent); new tokens start with `<app>-`.
pub fn open_keys(app: &str, library: &LibraryId) -> Result<KeyStore> {
    Ok(KeyStore::open(&keys_dir_for(app, library)?)?.token_prefix(&format!("{app}-")))
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

/// `<the app's cache>/<library id>/`: `<cache_dir>/<app>` on Linux and macOS, `<app>\cache` on
/// Windows, where the cache root is the data root (corundum's `AppDirs::cache`). Falls back to
/// `$HOME/.cache/<app>` when the platform has no cache dir.
pub fn cache_dir_for(app: &str, library: &LibraryId) -> Result<PathBuf> {
    let base = corundum_framework::platform::dirs::AppDirs::new(app)
        .map(|d| d.cache().to_path_buf())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".cache").join(app))
        })
        .ok_or_else(|| ServerError::Cache("cannot determine a cache directory".into()))?;
    Ok(base.join(library.0.hyphenated().to_string()))
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
