//! The Asteria Photos server: `init`, `token add` and `serve`.

use std::error::Error;
use std::fs::{File, TryLockError};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::{Duration, SystemTime};

use asteria_framework::fsutil::cleanup_tmp_files;
use asteria_framework::paths::META_DIR;
use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_server::upload::UploadStore;
use asteria_framework_server::watch::{DEFAULT_DEBOUNCE, DEFAULT_PERIOD};
use asteria_framework_server::{
    AppExtension, AppState, Cache, Processor, ScanLoop, ServerConfig, TokenEntry, Tokens,
    cache_dir_for, data_dir_for, new_token, router, spawn_scan_loop, spawn_writer, token_hash,
};
use asteria_photos_api::{APP, APP_API};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type AnyResult<T> = Result<T, Box<dyn Error>>;

const DEFAULT_LISTEN: &str = "127.0.0.1:7810";
const RENDER_JOBS: usize = 2;
const RENDER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long open connections get to finish once shutdown starts.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How often stale upload sessions are looked for.
const PURGE_PERIOD: Duration = Duration::from_secs(60 * 60);
const LOCK_FILE: &str = "server.lock";

#[derive(Parser)]
#[command(version, about = "Asteria Photos server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create (or adopt) a library and a config file, and print the owner's first token.
    Init {
        /// Library directory (created if missing).
        #[arg(long)]
        library: PathBuf,
        /// Owner's name, used when the library is created.
        #[arg(long)]
        owner: String,
        /// Config file to write.
        #[arg(long)]
        config: PathBuf,
        /// Address to listen on.
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: SocketAddr,
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Manage access tokens.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Run the server.
    Serve {
        #[arg(long)]
        config: PathBuf,
        /// Override the configured listen address.
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Add a token for a user listed in the library's users.toml and print it.
    Add {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        user: UserId,
    },
}

/// Photos shows images and videos.
struct PhotosExtension;

impl AppExtension for PhotosExtension {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init {
            library,
            owner,
            config,
            listen,
            force,
        } => init(&library, &owner, &config, listen, force),
        Command::Token {
            command: TokenCommand::Add { config, user },
        } => token_add(&config, user),
        Command::Serve { config, listen } => serve(&config, listen),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn init(
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
    std::fs::create_dir_all(library)?;
    let root = library.canonicalize()?;
    let lib = match Library::init(&root, owner) {
        Ok(lib) => {
            eprintln!("created library at {}", root.display());
            lib
        }
        Err(asteria_framework::Error::AlreadyInitialized(_)) => {
            eprintln!("using existing library at {}", root.display());
            Library::open(&root)?
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(parent) = config_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let token = new_token();
    let cfg = ServerConfig {
        library: root,
        listen,
        token: vec![TokenEntry {
            user: lib.owner(),
            hash: token_hash(&token),
        }],
    };
    cfg.save(config_path)?;
    eprintln!("wrote {}", config_path.display());
    println!("{token}");
    Ok(())
}

fn token_add(config_path: &Path, user: UserId) -> AnyResult<()> {
    let mut cfg = ServerConfig::load(config_path)?;
    let lib = Library::open(&cfg.library.canonicalize()?)?;
    if !lib.users().user.iter().any(|u| u.id == user) {
        return Err(format!("user {user} is not in the library's users.toml").into());
    }
    let token = new_token();
    cfg.token.push(TokenEntry {
        user,
        hash: token_hash(&token),
    });
    cfg.save(config_path)?;
    println!("{token}");
    Ok(())
}

fn serve(config_path: &Path, listen: Option<SocketAddr>) -> AnyResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cfg = ServerConfig::load(config_path)?;
    let listen = listen.unwrap_or(cfg.listen);
    let root = cfg.library.canonicalize()?;
    let library = Library::open(&root)?;
    // This instance's own state (journal, upload sessions), apart from the workspace, which
    // instances on other hosts may share.
    let data_dir = data_dir_for(APP, &library.config().id)?;
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
    let cache_dir = cache_dir_for(APP, &library.config().id)?;
    std::fs::create_dir_all(&cache_dir)?;
    let cache_dir = cache_dir.canonicalize()?;
    let cache = Arc::new(Cache::open(&cache_dir, &library.config().id)?);
    let thumbs = cache_dir.join("thumbs");
    if !thumbs.is_absolute() || thumbs.to_string_lossy().contains('%') {
        return Err(format!("unusable thumbnail directory {}", thumbs.display()).into());
    }
    let processor = Arc::new(Processor::new(thumbs, RENDER_JOBS, RENDER_TIMEOUT));
    processor.sweep_tmp();
    let tokens = Arc::new(Tokens::from_config(&cfg));
    let uploads = Arc::new(UploadStore::open(&data_dir)?);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(run(library, cache, processor, tokens, uploads, listen));
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
    library: Arc<Library>,
    cache: Arc<Cache>,
    processor: Arc<Processor>,
    tokens: Arc<Tokens>,
    uploads: Arc<UploadStore>,
    listen: SocketAddr,
) -> AnyResult<()> {
    let (writer, writer_handle) = spawn_writer(library.clone(), cache.clone());
    let closer = writer.clone();
    let scanning = Arc::new(AtomicBool::new(false));
    let (rescan_tx, rescan_rx) = mpsc::channel(1);
    let extension: Arc<dyn AppExtension> = Arc::new(PhotosExtension);
    let state = AppState::new(
        library.clone(),
        cache.clone(),
        writer.clone(),
        tokens,
        APP,
        APP_API,
        scanning.clone(),
        rescan_tx,
        extension.clone(),
        processor,
        uploads.clone(),
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let shutdown = CancellationToken::new();
    let app = router(state);
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
        extension,
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
