//! The desktop window: where the app's files are, and opening it. Desktop only; a phone
//! build starts the app its own way.

use std::error::Error;

use corundum_framework::gui::store::Store;
use corundum_framework::gui::window::{self, WindowOptions};
use corundum_framework::platform::dirs::AppDirs;

use crate::app::{AppPaths, MusicApp};
use crate::player::Player;
use crate::player::feed::HttpOpener;
use crate::player::output::CpalOutputs;

/// The app's name, for its directories.
pub const APP_NAME: &str = "asteria-music";

/// Where the app keeps `servers.toml` and `playback.toml` (its config directory) and the
/// cover cache and the streamed tracks (its cache directory).
pub fn paths(dirs: &AppDirs) -> AppPaths {
    AppPaths {
        servers: dirs.config().join("servers.toml"),
        playback: dirs.config().join("playback.toml"),
        cache: Some(dirs.cache().to_path_buf()),
    }
}

/// Sends the app's log to stderr: warnings and errors unless `RUST_LOG` says otherwise
/// (`RUST_LOG=asteria_music=debug`, say). Nothing the app logs carries a token: tokens only
/// travel in the `Authorization` header, and their `Debug` output is masked.
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // Fails only if a subscriber is already installed, which then keeps logging.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Open the window and run the app until it closes.
pub fn run() -> Result<(), Box<dyn Error>> {
    init_logging();
    let dirs = AppDirs::new(APP_NAME).ok_or("this system has no config or cache directory")?;
    let paths = paths(&dirs);
    let options = WindowOptions::new(APP_NAME).remember(Store::new(&dirs));
    window::run(options, move |cc| {
        let player = Player::spawn(
            Box::new(HttpOpener::new()),
            Box::new(CpalOutputs),
            cc.egui_ctx.clone(),
        )
        // The window cannot report an app that fails to start; this is the OS refusing a thread.
        .expect("starting the playback thread failed");
        Box::new(MusicApp::new(paths, &cc.egui_ctx, player))
    })?;
    Ok(())
}
