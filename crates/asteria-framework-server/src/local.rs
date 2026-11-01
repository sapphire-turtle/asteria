//! Setting up a server on this host for the app's own use: a library, a config and an API key.
//! The desktop does this on first run; `init` shares its library step.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use asteria_framework::{Library, LibraryId};
use corundum_framework::keys::KeyStore;

use crate::cli::AppSpec;
use crate::config::free_name;
use crate::error::{Result, ServerError};
use crate::{ServerConfig, default_config_path, open_keys};

/// A local server that is set up: where its config is, where it listens, and the token of a
/// key made for the caller.
#[derive(Clone)]
pub struct LocalServer {
    pub config: PathBuf,
    pub listen: SocketAddr,
    /// Shown once; only its hash is kept.
    pub token: String,
}

/// Leaves the token out.
impl std::fmt::Debug for LocalServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalServer")
            .field("config", &self.config)
            .field("listen", &self.listen)
            .field("token", &"…")
            .finish()
    }
}

/// Sets up the local server at the default config path: see [`setup_local_at`]. The keys are
/// the library's in the data directory.
pub fn setup_local(
    spec: &AppSpec,
    library: &Path,
    owner: &str,
    key_name: &str,
) -> Result<LocalServer> {
    setup_local_at(
        spec,
        &default_config_path(spec.app)?,
        library,
        owner,
        key_name,
        |id| open_keys(spec.app, id),
    )
}

/// Sets up the local server with its config at `config`; `keys` opens the store of the
/// library's keys (the data directory's, for [`setup_local`]).
///
/// With no config there, `library` is created if missing and made a library (an existing one
/// is opened as it is, keeping its id), and the config is written with the app's default
/// listen address. A config that exists is read and left as it is; if it names another
/// library than `library`, that is an error and nothing is written. Either way a key called
/// `key_name` (`key_name-2`, `-3`… when that is in use) is added, and its token returned.
pub fn setup_local_at(
    spec: &AppSpec,
    config: &Path,
    library: &Path,
    owner: &str,
    key_name: &str,
    keys: impl FnOnce(&LibraryId) -> Result<KeyStore>,
) -> Result<LocalServer> {
    let (lib, listen) = if config.exists() {
        let cfg = crate::load_and_migrate(spec.app, config)?;
        let configured = cfg.library.canonicalize().map_err(|e| {
            ServerError::Config(format!(
                "the library {} named in {} cannot be found ({e}); reconnect it, or edit or \
                 remove {}",
                cfg.library.display(),
                config.display(),
                config.display()
            ))
        })?;
        if library.canonicalize().ok().as_ref() != Some(&configured) {
            return Err(ServerError::Config(format!(
                "a local server for the library {} is already set up in {}, not for {}: \
                 choose {}, or edit or remove {}",
                configured.display(),
                config.display(),
                library.display(),
                configured.display(),
                config.display()
            )));
        }
        (Library::open(&configured)?, cfg.listen)
    } else {
        let listen: SocketAddr = spec.default_listen.parse().map_err(|e| {
            ServerError::Config(format!("default listen {:?}: {e}", spec.default_listen))
        })?;
        let (root, lib, _) = open_or_init_library(library, owner)?;
        if let Some(parent) = config.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| ServerError::Config(format!("{}: {e}", parent.display())))?;
        }
        ServerConfig {
            library: root,
            listen,
        }
        .save(config)?;
        (lib, listen)
    };
    let mut keys = keys(&lib.config().id)?;
    let name = unique_name(&keys, key_name);
    let (_, token) = keys.add(&name, None, None)?;
    Ok(LocalServer {
        config: config.to_path_buf(),
        listen,
        token: token.expose().to_owned(),
    })
}

/// Creates `library` (and the directory) or, when it is one already, opens it. The path made
/// absolute, the library, and whether it was created.
pub(crate) fn open_or_init_library(
    library: &Path,
    owner: &str,
) -> Result<(PathBuf, Library, bool)> {
    std::fs::create_dir_all(library)?;
    let root = library.canonicalize()?;
    match Library::init(&root, owner) {
        Ok(lib) => Ok((root, lib, true)),
        Err(asteria_framework::Error::AlreadyInitialized(_)) => {
            let lib = Library::open(&root)?;
            Ok((root, lib, false))
        }
        Err(e) => Err(e.into()),
    }
}

/// `base` if no unretired key is called that, else `base-2`, `base-3`…
pub(crate) fn unique_name(keys: &KeyStore, base: &str) -> String {
    if keys
        .list()
        .iter()
        .any(|k| k.name == base && !k.is_retired())
    {
        free_name(keys, &format!("{base}-"), 2)
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppExtension,
        keys::{KeyVerifier, Verdict},
    };
    use std::sync::Arc;

    struct Nothing;
    impl AppExtension for Nothing {
        fn accepts(&self, _: asteria_framework::MediaKind) -> bool {
            false
        }
    }

    fn spec() -> AppSpec {
        AppSpec {
            app: "asteria-test",
            app_api: 1,
            about: "test",
            default_listen: "127.0.0.1:7899",
            extension: Arc::new(Nothing),
        }
    }

    /// A temp dir with a `keys` directory for the library's keys.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn keys_dir(&self) -> PathBuf {
            self.dir.path().join("keys")
        }
        fn config(&self) -> PathBuf {
            self.dir.path().join("conf/server.toml")
        }
        fn setup(&self, library: &Path, key: &str) -> Result<LocalServer> {
            let keys = self.keys_dir();
            setup_local_at(&spec(), &self.config(), library, "me", key, move |_| {
                Ok(KeyStore::open(&keys)?.token_prefix("asteria-test-"))
            })
        }
        fn accepts(&self, token: &str) -> bool {
            matches!(
                KeyVerifier::new(&self.keys_dir()).verify(token),
                Verdict::Accepted(_)
            )
        }
    }

    #[test]
    fn a_new_library_gets_a_config_and_a_key() {
        let f = Fixture::new();
        let lib = f.dir.path().join("lib");
        let server = f.setup(&lib, "desktop").unwrap();
        assert_eq!(server.config, f.config());
        assert_eq!(server.listen, "127.0.0.1:7899".parse().unwrap());
        assert!(lib.join(".asteria/library.toml").is_file());
        let cfg = ServerConfig::load(&f.config()).unwrap();
        assert_eq!(cfg.library, lib.canonicalize().unwrap());
        assert_eq!(cfg.listen, server.listen);
        assert!(server.token.starts_with("asteria-test-"));
        assert!(f.accepts(&server.token));
    }

    #[test]
    fn an_existing_library_keeps_its_id() {
        let f = Fixture::new();
        let lib = f.dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        let id = Library::init(&lib.canonicalize().unwrap(), "me")
            .unwrap()
            .config()
            .id;
        f.setup(&lib, "desktop").unwrap();
        assert_eq!(
            Library::open(&lib.canonicalize().unwrap())
                .unwrap()
                .config()
                .id,
            id
        );
    }

    #[test]
    fn an_existing_config_is_not_overwritten() {
        let f = Fixture::new();
        let lib = f.dir.path().join("lib");
        let first = f.setup(&lib, "desktop").unwrap();
        let before = std::fs::read(f.config()).unwrap();
        let second = f.setup(&lib, "desktop").unwrap();
        assert_eq!(std::fs::read(f.config()).unwrap(), before);
        assert_ne!(first.token, second.token);
        assert!(f.accepts(&first.token) && f.accepts(&second.token));
        let keys = KeyStore::open(&f.keys_dir()).unwrap();
        let mut names: Vec<_> = keys.list().iter().map(|k| k.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["desktop", "desktop-2"]);
    }

    #[test]
    fn another_librarys_config_is_refused() {
        let f = Fixture::new();
        f.setup(&f.dir.path().join("lib"), "desktop").unwrap();
        let before = std::fs::read(f.config()).unwrap();
        let other = f.dir.path().join("other");
        let err = f.setup(&other, "desktop").unwrap_err().to_string();
        let configured = f.dir.path().join("lib").canonicalize().unwrap();
        for part in [
            format!(
                "a local server for the library {} is already set up",
                configured.display()
            ),
            format!("not for {}", other.display()),
            format!(
                "choose {}, or edit or remove {}",
                configured.display(),
                f.config().display()
            ),
        ] {
            assert!(err.contains(&part), "{part:?} not in {err}");
        }
        assert!(!other.exists());
        assert_eq!(std::fs::read(f.config()).unwrap(), before);
        assert_eq!(KeyStore::open(&f.keys_dir()).unwrap().list().len(), 1);
    }

    #[test]
    fn a_configured_library_that_is_gone_is_named() {
        let f = Fixture::new();
        let lib = f.dir.path().join("lib");
        f.setup(&lib, "desktop").unwrap();
        let configured = lib.canonicalize().unwrap();
        std::fs::remove_dir_all(&lib).unwrap();
        let err = f.setup(&lib, "desktop").unwrap_err().to_string();
        assert!(!err.contains("another library"), "{err}");
        for part in [
            format!("the library {} named in", configured.display()),
            "cannot be found".to_owned(),
            format!("edit or remove {}", f.config().display()),
        ] {
            assert!(err.contains(&part), "{part:?} not in {err}");
        }
    }

    #[test]
    fn debug_does_not_show_the_token() {
        let f = Fixture::new();
        let server = f.setup(&f.dir.path().join("lib"), "desktop").unwrap();
        let shown = format!("{server:?}");
        assert!(!shown.contains(&server.token), "{shown}");
        assert!(shown.contains("token: \"…\""), "{shown}");
        assert!(shown.contains(&server.listen.to_string()), "{shown}");
    }
}
