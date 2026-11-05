//! The command line shared by the Asteria servers: `init`, `api-key`, `serve` and `update`, with the
//! serve runtime (instance lock, upload purge, shutdown). An app supplies an [`AppSpec`].

use std::error::Error;
use std::fs::{File, TryLockError};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::{Duration, SystemTime};

use asteria_common_core::Library;
use asteria_common_core::fsutil::cleanup_tmp_files;
use asteria_common_core::paths::META_DIR;
use clap::{Arg, ArgAction, ArgMatches, Args, Command, FromArgMatches, Subcommand, value_parser};
use corundum_framework::keys::{KeyCommand, KeyId, KeyStore, KeyVerifier, ProfileId, Token};
use corundum_framework_update::{ReleaseSource, UpdateArgs};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::local::{open_or_init_library, unique_name};
use crate::upload::UploadStore;
use crate::watch::{DEFAULT_DEBOUNCE, DEFAULT_PERIOD};
use crate::{
    AppExtension, AppState, Cache, Processor, ScanLoop, ServerConfig, cache_dir_for, data_dir_for,
    default_config_path, keys_dir_for, load_and_migrate, open_keys, router_with_shutdown,
    spawn_scan_loop, spawn_writer,
};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const RENDER_JOBS: usize = 2;
const RENDER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long open connections get to finish once shutdown starts.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How often stale upload sessions are looked for.
const PURGE_PERIOD: Duration = Duration::from_secs(60 * 60);
const LOCK_FILE: &str = "server.lock";
/// `--owner`: a valid user name.
fn username(s: &str) -> Result<String, String> {
    if asteria_common_core::library::is_valid_username(s) {
        Ok(s.to_owned())
    } else {
        Err(asteria_common_core::Error::InvalidUsername(s.to_owned()).to_string())
    }
}

/// The key `init` makes for the owner.
const OWNER_KEY: &str = "owner";

/// What differs between the apps' servers.
pub struct AppSpec {
    /// The app's name, e.g. `asteria-photos`; names the data and cache directories.
    pub app: &'static str,
    pub app_api: u32,
    /// The command line's `about`.
    pub about: &'static str,
    /// Address `init` writes to the config file unless `--listen` is given.
    pub default_listen: &'static str,
    pub extension: Arc<dyn AppExtension>,
    /// The release file base this binary updates itself from (`asteria-photos-headless`);
    /// `None` for a build that is updated by its installer.
    pub release_asset: Option<&'static str>,
}

/// Where this program's newer releases are published.
fn release_source(spec: &AppSpec) -> ReleaseSource {
    let source = ReleaseSource::github("sapphire-turtle", "asteria");
    match spec.release_asset {
        Some(base) => source.asset(base),
        None => source,
    }
}

fn command(spec: &AppSpec) -> Command {
    // The binary is named after the app; clap keeps names for the process's lifetime.
    let name: &'static str = Box::leak(format!("{}-server", spec.app).into_boxed_str());
    let config = || {
        Arg::new("config")
            .long("config")
            .value_name("CONFIG")
            .value_parser(value_parser!(PathBuf))
            .help(format!(
                "Config file [default: <config dir>/{}/server.toml]",
                spec.app
            ))
    };
    Command::new(name)
        .version(env!("CARGO_PKG_VERSION"))
        .about(spec.about)
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("init")
                .about("Create (or adopt) a library and a config file, and print the token of the API key `owner`")
                .arg(
                    Arg::new("library")
                        .long("library").value_name("LIBRARY")
                        .required(true)
                        .value_parser(value_parser!(PathBuf))
                        .help("Library directory (created if missing)"),
                )
                .arg(
                    Arg::new("owner")
                        .long("owner").value_name("USER")
                        .required(true)
                        .value_parser(username)
                        .help("The owner's user name (ASCII letters, digits, - and _), used when the library is created"),
                )
                .arg(config())
                .arg(
                    Arg::new("listen")
                        .long("listen").value_name("LISTEN")
                        .default_value(spec.default_listen)
                        .value_parser(value_parser!(SocketAddr))
                        .help("Address to listen on"),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Overwrite an existing config file"),
                ),
        )
        .subcommand(KeyCommand::augment_subcommands(
            Command::new("api-key")
                .about("Manage the API keys; a key acts as the user its profile names")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .arg(config().global(true)),
        ))
        .subcommand(
            // An earlier version's `token`, kept out of the help: points to `api-key`.
            Command::new("token")
                .hide(true)
                .disable_help_flag(true)
                .arg(
                    Arg::new("args")
                        .num_args(0..)
                        .allow_hyphen_values(true)
                        .trailing_var_arg(true),
                ),
        )
        .subcommand(UpdateArgs::augment_args(
            Command::new("update").about("Replace this program with its newest release"),
        ))
        .subcommand(
            Command::new("serve")
                .about("Run the server")
                .arg(config())
                .arg(
                    Arg::new("listen")
                        .long("listen").value_name("LISTEN")
                        .value_parser(value_parser!(SocketAddr))
                        .help("Override the configured listen address"),
                ),
        )
}

/// The `--config` given, or `<config dir>/<app>/server.toml`.
fn config_path(spec: &AppSpec, m: &ArgMatches) -> AnyResult<PathBuf> {
    match m.get_one::<PathBuf>("config") {
        Some(path) => Ok(path.clone()),
        None => Ok(default_config_path(spec.app)?),
    }
}

/// The name this program was run by (`asteria-photos`, `asteria-photos-desktop`), else the
/// app's.
fn program_name(spec: &AppSpec) -> String {
    std::env::args_os()
        .next()
        .and_then(|a| {
            Path::new(&a)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| spec.app.to_owned())
}

/// Runs the command; the process's exit code.
fn dispatch(spec: &AppSpec, matches: &ArgMatches) -> AnyResult<i32> {
    match matches.subcommand() {
        Some(("init", m)) => init(
            spec,
            m.get_one::<PathBuf>("library").unwrap(),
            m.get_one::<String>("owner").unwrap(),
            &config_path(spec, m)?,
            *m.get_one::<SocketAddr>("listen").unwrap(),
            m.get_flag("force"),
        )
        .map(|()| 0),
        Some(("api-key", m)) => api_key(
            spec,
            &config_path(spec, m)?,
            KeyCommand::from_arg_matches(m)?,
        ),
        Some(("token", _)) => {
            eprintln!(
                "error: `token` was replaced by `api-key`: run `{} api-key add <name> --profile \
                 <user name>`",
                program_name(spec)
            );
            Ok(2)
        }
        Some(("serve", m)) => serve(
            spec,
            &config_path(spec, m)?,
            m.get_one::<SocketAddr>("listen").copied(),
        )
        .map(|()| 0),
        Some(("update", m)) => update(spec, &UpdateArgs::from_arg_matches(m)?),
        _ => unreachable!("subcommand_required"),
    }
}

/// Replaces this program with the newest release, or says how its build is updated instead.
/// The process's exit code.
fn update(spec: &AppSpec, args: &UpdateArgs) -> AnyResult<i32> {
    let program = program_name(spec);
    if spec.release_asset.is_none() && !args.check {
        eprintln!(
            "{program} is updated by the app it came with (Settings \u{203a} About), or by \
             reinstalling it"
        );
        return Ok(1);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // `run` says to restart a running server after it replaced this program.
    Ok(runtime.block_on(corundum_framework_update::run(
        args,
        &release_source(spec),
        &program,
        env!("CARGO_PKG_VERSION"),
    ))?)
}

/// Parses the command line (init / api-key / serve / update), moves the app's directories from
/// Roaming to Local on Windows, and runs it; exits the process with 1 on error.
pub fn main(spec: AppSpec) -> ! {
    let matches = command(&spec).get_matches();
    // Before any data or cache directory is opened: on Windows they move from Roaming to Local.
    let migrated = asteria_common_core::dirs::migrate_dirs(spec.app, |a| {
        use corundum_framework::platform::dirs::{AppDirs, Migrated};
        AppDirs::migrate_from_roaming(a).map(|m| match m {
            Migrated::Moved { left } | Migrated::Merged { left } => left,
            Migrated::Nothing => Vec::new(),
        })
    });
    match migrated {
        Ok(left) => asteria_common_core::dirs::report_left(spec.app, &left),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
    match dispatch(&spec, &matches) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Creates or adopts the library, adds the API key `owner` (`owner-2`, `owner-3`… when that
/// name is taken), writes the config, and prints the key's token, alone, on stdout.
fn init(
    spec: &AppSpec,
    library: &Path,
    owner: &str,
    config_path: &Path,
    listen: SocketAddr,
    force: bool,
) -> AnyResult<()> {
    if config_path.exists() && !force {
        return Err(format!(
            "{} already exists (use --force to overwrite it)",
            config_path.display()
        )
        .into());
    }
    let (root, lib, created) = open_or_init_library(library, owner)?;
    if created {
        eprintln!("created library at {}", root.display());
    } else {
        eprintln!("using existing library at {}", root.display());
    }
    if let Some(parent) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut keys = open_keys(spec.app, &lib)?;
    let token = owner_key(&mut keys, crate::profile::owner_profile(&lib)?, force)?;
    let cfg = ServerConfig {
        library: root,
        listen,
    };
    cfg.save(config_path)?;
    eprintln!("wrote {}", config_path.display());
    println!("{}", token.expose());
    Ok(())
}

/// The key `init` makes. With `force`, every unretired key is retired first, so that only the
/// new one works (as `--force` replaced every token when they were in the config; an older
/// config's `[[token]]` entries are not imported, and go with the file). Without it no key is
/// touched: when `owner` is taken, the new key is `owner-2`, `owner-3`…
fn owner_key(keys: &mut KeyStore, owner: ProfileId, force: bool) -> AnyResult<Token> {
    if force {
        let live: Vec<KeyId> = keys
            .list()
            .iter()
            .filter(|k| !k.is_retired())
            .map(|k| k.id.clone())
            .collect();
        for id in &live {
            keys.retire(id.as_str())?;
        }
        eprintln!(
            "retired {} existing keys; only the new owner key works now",
            live.len()
        );
    }
    let name = unique_name(keys, OWNER_KEY);
    if name != OWNER_KEY {
        eprintln!("a key `{OWNER_KEY}` exists already; the new key is `{name}`");
    }
    Ok(keys.add(&name, None, owner)?.1)
}

/// Runs `command` on the API keys of the library the config at `config_path` names.
fn api_key(spec: &AppSpec, config_path: &Path, command: KeyCommand) -> AnyResult<i32> {
    let cfg = load_and_migrate(spec.app, config_path)?;
    let lib = Library::open(&library_root(&cfg, config_path)?)?;
    let mut keys = open_keys(spec.app, &lib)?;
    Ok(command.run(&mut keys, &crate::profile::profiles(&lib))?)
}

/// The library the config names, as an absolute path; an error names both.
fn library_root(cfg: &ServerConfig, config_path: &Path) -> AnyResult<PathBuf> {
    cfg.library.canonicalize().map_err(|e| {
        format!(
            "library {} (named in {}): {e}",
            cfg.library.display(),
            config_path.display()
        )
        .into()
    })
}

fn serve(spec: &AppSpec, config_path: &Path, listen: Option<SocketAddr>) -> AnyResult<()> {
    // Fails only if the embedding program installed a subscriber already, which then logs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let cfg = load_and_migrate(spec.app, config_path)?;
    let listen = listen.unwrap_or(cfg.listen);
    let root = library_root(&cfg, config_path)?;
    let library = Library::open(&root)?;
    // This instance's own state (journal, upload sessions), apart from the workspace, which
    // instances on other hosts may share.
    let data_dir = data_dir_for(spec.app, &library.config().id)?;
    std::fs::create_dir_all(&data_dir)?;
    let data_dir = data_dir.canonicalize()?;
    // Before touching anything: another server on this host may be serving this library
    // with the same data and cache directories. Held until the process exits.
    let _lock = lock_instance(&data_dir, &root)?;
    let legacy = legacy_state(&root);
    if !legacy.is_empty() {
        tracing::info!(
            found = ?legacy,
            data_dir = %data_dir.display(),
            "the library's .asteria directory holds state of an older or other server; \
             it is left as is, and this server keeps its own in its data directory"
        );
    }
    let library = Arc::new(library.with_state_dir(&data_dir));
    let removed = cleanup_tmp_files(library.root())?;
    if removed > 0 {
        tracing::info!(removed, "removed leftover temporary files");
    }
    let cache_dir = cache_dir_for(spec.app, &library.config().id)?;
    std::fs::create_dir_all(&cache_dir)?;
    let cache_dir = cache_dir.canonicalize()?;
    let cache = Arc::new(Cache::open(&cache_dir, &library.config().id)?);
    let thumbs = cache_dir.join("thumbs");
    if !thumbs.is_absolute() || thumbs.to_string_lossy().contains('%') {
        return Err(format!("unusable thumbnail directory {}", thumbs.display()).into());
    }
    let processor = Arc::new(Processor::new(thumbs, RENDER_JOBS, RENDER_TIMEOUT));
    processor.sweep_tmp();
    // Created when absent, so the verifier finds a directory; keys added to it later (by
    // `api-key add`, another host, a sync) work without a restart.
    let keys = open_keys(spec.app, &library)?;
    if !keys.list().iter().any(|k| !k.is_retired()) {
        tracing::warn!(
            "there are no API keys yet; add one with `api-key add <name> --profile <user name>`"
        );
    }
    let keys = Arc::new(KeyVerifier::new(&keys_dir_for(
        spec.app,
        &library.config().id,
    )?));
    let uploads = Arc::new(UploadStore::open(&data_dir)?);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(run(spec, library, cache, processor, keys, uploads, listen));
    // Connections cut off after the grace period may still have tasks; do not wait for them.
    rt.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Takes the exclusive lock `<data dir>/server.lock`, so that one server at a time on this
/// host serves the library at `root` (servers on other hosts have their own data directories
/// and may share the workspace). The lock lasts as long as the returned file is open.
fn lock_instance(data_dir: &Path, root: &Path) -> AnyResult<File> {
    let path = data_dir.join(LOCK_FILE);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(format!(
            "another asteria server is serving this library ({})",
            root.display()
        )
        .into()),
        Err(TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display()).into()),
    }
}

async fn run(
    spec: &AppSpec,
    library: Arc<Library>,
    cache: Arc<Cache>,
    processor: Arc<Processor>,
    keys: Arc<KeyVerifier>,
    uploads: Arc<UploadStore>,
    listen: SocketAddr,
) -> AnyResult<()> {
    let (writer, writer_handle) = spawn_writer(library.clone(), cache.clone());
    let closer = writer.clone();
    let scanning = Arc::new(AtomicBool::new(false));
    let (rescan_tx, rescan_rx) = mpsc::channel(1);
    let state = AppState::new(
        library.clone(),
        cache.clone(),
        writer.clone(),
        keys,
        spec.app,
        spec.app_api,
        scanning.clone(),
        rescan_tx,
        spec.extension.clone(),
        processor,
        uploads.clone(),
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let shutdown = CancellationToken::new();
    let app = router_with_shutdown(state, shutdown.clone());
    let token = shutdown.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
    });
    let purge = tokio::spawn(purge_loop(uploads, shutdown.clone()));
    let mut scan = spawn_scan_loop(ScanLoop {
        library,
        cache: cache.clone(),
        extension: spec.extension.clone(),
        writer,
        scanning,
        trigger: rescan_rx,
        debounce: DEFAULT_DEBOUNCE,
        period: DEFAULT_PERIOD,
        shutdown: shutdown.clone(),
        passes: Arc::new(AtomicUsize::new(0)),
    });

    // What ended the wait; a signal is a clean stop, a finished task is a failure.
    let mut failure: Option<Box<dyn Error>> = None;
    let mut server_done = false;
    let mut scan_done = false;
    tokio::select! {
        r = wait_for_signal() => {
            if let Err(e) = r {
                failure = Some(e.into());
            }
            tracing::info!("shutting down");
        }
        r = &mut server => {
            server_done = true;
            failure = Some(match r {
                Ok(Ok(())) => "server stopped unexpectedly".into(),
                Ok(Err(e)) => e.into(),
                Err(e) => e.into(),
            });
        }
        r = &mut scan => {
            scan_done = true;
            failure = Some(match r {
                Ok(()) => "scan loop stopped unexpectedly".into(),
                Err(e) => e.into(),
            });
        }
    }
    shutdown.cancel();
    if !server_done {
        match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
            Ok(r) => {
                if let Err(e) = flatten(r) {
                    failure.get_or_insert(e);
                }
            }
            Err(_) => {
                tracing::warn!(
                    "connections still open after {}s; closing them",
                    SHUTDOWN_GRACE.as_secs()
                );
                server.abort();
            }
        }
    }
    purge.abort();
    if !scan_done && let Err(e) = scan.await {
        failure.get_or_insert(e.into());
    }
    // Connection tasks cut off above may still hold Writer clones: close the queue so it
    // ends once the queued jobs are written.
    closer.close().await;
    drop(closer);
    if let Err(e) = writer_handle.await {
        failure.get_or_insert(e.into());
    }
    if let Err(e) = cache.flush() {
        failure.get_or_insert(e.into());
    }
    match failure {
        Some(e) => Err(e),
        None => {
            tracing::info!("stopped");
            Ok(())
        }
    }
}

/// What `<library>/.asteria/` holds of a server's own state: written there by older
/// versions, or by a server of another host that still does. Never touched, only reported.
fn legacy_state(root: &Path) -> Vec<&'static str> {
    ["journal", "uploads", LOCK_FILE]
        .into_iter()
        .filter(|name| root.join(META_DIR).join(name).exists())
        .collect()
}

/// Removes stale upload sessions at startup and then every `PURGE_PERIOD`.
async fn purge_loop(uploads: Arc<UploadStore>, shutdown: CancellationToken) {
    let mut tick = tokio::time::interval(PURGE_PERIOD);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = shutdown.cancelled() => return,
        }
        let store = uploads.clone();
        match tokio::task::spawn_blocking(move || store.purge_expired(SystemTime::now())).await {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "removed stale upload sessions"),
            Err(e) => tracing::error!(error = %e, "upload purge failed"),
        }
    }
}

fn flatten(r: Result<std::io::Result<()>, tokio::task::JoinError>) -> Result<(), Box<dyn Error>> {
    r?.map_err(Into::into)
}

/// Resolves on Ctrl-C or (on Unix) SIGTERM.
async fn wait_for_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;
    impl AppExtension for Nothing {
        fn accepts(&self, _: asteria_common_core::MediaKind) -> bool {
            false
        }
    }

    fn spec() -> AppSpec {
        AppSpec {
            app: "asteria-test",
            app_api: 1,
            about: "test",
            default_listen: "127.0.0.1:0",
            extension: Arc::new(Nothing),
            release_asset: None,
        }
    }

    #[test]
    fn update_takes_check_and_yes() {
        let m = command(&spec())
            .try_get_matches_from(["x", "update", "--check", "-y"])
            .unwrap();
        let (name, m) = m.subcommand().unwrap();
        assert_eq!(name, "update");
        let args = UpdateArgs::from_arg_matches(m).unwrap();
        assert!(args.check && args.yes);
    }

    #[test]
    fn only_a_binary_with_a_release_asset_replaces_itself() {
        let mut s = spec();
        assert_eq!(release_source(&s).asset_base(), None);
        s.release_asset = Some("asteria-photos-headless");
        assert_eq!(
            release_source(&s).asset_base(),
            Some("asteria-photos-headless")
        );
        assert_eq!(release_source(&s).repository(), "sapphire-turtle/asteria");
    }

    #[test]
    fn a_build_without_a_release_asset_points_to_its_installer() {
        let args = UpdateArgs {
            check: false,
            yes: true,
        };
        assert_eq!(update(&spec(), &args).unwrap(), 1);
    }

    #[test]
    fn config_is_optional_and_defaults_to_the_apps_config_dir() {
        let spec = spec();
        let m = command(&spec).try_get_matches_from(["x", "serve"]).unwrap();
        let (_, m) = m.subcommand().unwrap();
        assert_eq!(
            config_path(&spec, m).unwrap(),
            default_config_path("asteria-test").unwrap()
        );
    }

    fn store() -> (tempfile::TempDir, KeyStore) {
        let dir = tempfile::tempdir().unwrap();
        let keys = KeyStore::open(&dir.path().join("keys")).unwrap();
        (dir, keys)
    }

    fn me() -> ProfileId {
        ProfileId::new("me").unwrap()
    }

    #[test]
    fn owner_must_be_a_user_name() {
        let spec = spec();
        let args = |owner: &'static str| ["x", "init", "--library", "/l", "--owner", owner];
        assert!(command(&spec).try_get_matches_from(args("fluo10")).is_ok());
        let err = command(&spec)
            .try_get_matches_from(args("a b"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid user name \"a b\""), "{err}");
    }

    #[test]
    fn init_makes_the_owner_key() {
        let (_dir, mut keys) = store();
        let token = owner_key(&mut keys, me(), false).unwrap();
        assert_eq!(keys.authenticate(token.expose()).unwrap().name, OWNER_KEY);
    }

    /// Without `--force` no key stops working: a second `owner` is `owner-2`, then `owner-3`.
    #[test]
    fn init_without_force_keeps_every_key() {
        let (_dir, mut keys) = store();
        let first = owner_key(&mut keys, me(), false).unwrap();
        let second = owner_key(&mut keys, me(), false).unwrap();
        let third = owner_key(&mut keys, me(), false).unwrap();
        let name = |t: &Token| keys.authenticate(t.expose()).unwrap().name.clone();
        assert_eq!(name(&first), "owner");
        assert_eq!(name(&second), "owner-2");
        assert_eq!(name(&third), "owner-3");
    }

    /// `--force` revokes every key, imported old tokens included; only the new one works.
    #[test]
    fn init_with_force_retires_every_key() {
        let (dir, mut keys) = store();
        let old = owner_key(&mut keys, me(), false).unwrap();
        let (_, phone) = keys.add("phone", None, me()).unwrap();
        let config = dir.path().join("server.toml");
        let lib = dir.path().join("lib");
        Library::init(&lib, "me").unwrap();
        std::fs::write(
            &config,
            format!(
                "library = {:?}\nlisten = \"127.0.0.1:7810\"\n\n[[token]]\nuser = \"123abcd\"\nhash = \"{}\"\n",
                lib.display().to_string(),
                corundum_framework::keys::KeyHash::of_blake3("old-token")
            ),
        )
        .unwrap();
        crate::load_and_migrate_in(&dir.path().join("keys"), "asteria-test", &config).unwrap();
        let mut keys = KeyStore::open(&dir.path().join("keys")).unwrap();
        assert!(keys.authenticate("old-token").is_some());

        let new = owner_key(&mut keys, me(), true).unwrap();
        for refused in [old.expose(), phone.expose(), "old-token"] {
            assert!(keys.authenticate(refused).is_none());
        }
        assert_eq!(keys.authenticate(new.expose()).unwrap().name, OWNER_KEY);
        assert_eq!(keys.list().iter().filter(|k| !k.is_retired()).count(), 1);
    }

    /// `--config` goes before or after the key command.
    #[test]
    fn api_key_takes_config_on_either_side() {
        let spec = spec();
        for args in [
            [
                "x",
                "api-key",
                "--config",
                "/c.toml",
                "add",
                "phone",
                "--profile",
                "owner",
            ],
            [
                "x",
                "api-key",
                "add",
                "phone",
                "--profile",
                "owner",
                "--config",
                "/c.toml",
            ],
        ] {
            let m = command(&spec).try_get_matches_from(args).unwrap();
            let (name, m) = m.subcommand().unwrap();
            assert_eq!(name, "api-key");
            assert_eq!(config_path(&spec, m).unwrap(), Path::new("/c.toml"));
            assert!(matches!(
                KeyCommand::from_arg_matches(m).unwrap(),
                KeyCommand::Add { name, .. } if name == "phone"
            ));
        }
    }

    #[test]
    fn instance_lock_is_exclusive_until_released() {
        let (data, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let first = lock_instance(data.path(), root.path()).unwrap();
        let err = lock_instance(data.path(), root.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("another asteria server is serving this library"),
            "{err}"
        );
        drop(first);
        lock_instance(data.path(), root.path()).unwrap();
    }

    /// The workspace may be shared with servers on other hosts: the lock is not in it.
    #[test]
    fn instance_lock_is_in_the_data_dir() {
        let (data, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let _lock = lock_instance(data.path(), root.path()).unwrap();
        assert!(data.path().join(LOCK_FILE).is_file());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn legacy_state_in_the_workspace_is_found_and_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let meta = root.path().join(META_DIR);
        std::fs::create_dir_all(meta.join("uploads")).unwrap();
        std::fs::write(meta.join("library.toml"), "").unwrap();
        assert_eq!(legacy_state(root.path()), ["uploads"]);
        std::fs::create_dir(meta.join("journal")).unwrap();
        assert_eq!(legacy_state(root.path()), ["journal", "uploads"]);
        assert!(meta.join("journal").is_dir() && meta.join("uploads").is_dir());
    }
}
