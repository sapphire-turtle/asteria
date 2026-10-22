//! The Asteria gallery server: `init`, `token add` and `serve`.

use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use asteria_framework::fsutil::cleanup_tmp_files;
use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_server::watch::{DEFAULT_DEBOUNCE, DEFAULT_PERIOD};
use asteria_framework_server::{
    AppState, Cache, MediaFilter, Processor, ScanLoop, ServerConfig, TokenEntry, Tokens,
    cache_dir_for, new_token, router, spawn_scan_loop, spawn_writer, token_hash,
};
use asteria_gallery_api::{APP, APP_API};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type AnyResult<T> = Result<T, Box<dyn Error>>;

const DEFAULT_LISTEN: &str = "127.0.0.1:7810";
const RENDER_JOBS: usize = 2;
const RENDER_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(version, about = "Asteria gallery server")]
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

/// The gallery shows images and videos.
struct GalleryFilter;

impl MediaFilter for GalleryFilter {
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
    let library = Arc::new(Library::open(&root)?);
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

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(library, cache, processor, tokens, listen))
}

async fn run(
    library: Arc<Library>,
    cache: Arc<Cache>,
    processor: Arc<Processor>,
    tokens: Arc<Tokens>,
    listen: SocketAddr,
) -> AnyResult<()> {
    let (writer, writer_handle) = spawn_writer(library.clone(), cache.clone());
    let scanning = Arc::new(AtomicBool::new(false));
    let (rescan_tx, rescan_rx) = mpsc::channel(1);
    let filter: Arc<dyn MediaFilter> = Arc::new(GalleryFilter);
    let state = AppState::new(
        library.clone(),
        cache.clone(),
        writer.clone(),
        tokens,
        APP,
        APP_API,
        scanning.clone(),
        rescan_tx,
        filter.clone(),
        processor,
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let shutdown = CancellationToken::new();
    let app = router(state);
    let token = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
    });
    let scan = spawn_scan_loop(ScanLoop {
        library,
        cache: cache.clone(),
        filter,
        writer,
        scanning,
        trigger: rescan_rx,
        debounce: DEFAULT_DEBOUNCE,
        period: DEFAULT_PERIOD,
        shutdown: shutdown.clone(),
        passes: Arc::new(AtomicUsize::new(0)),
    });

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    shutdown.cancel();
    server.await??;
    scan.await?;
    // Every Writer clone is gone now (router/state and scan loop), so the queue drains.
    writer_handle.await?;
    cache.flush()?;
    tracing::info!("stopped");
    Ok(())
}
