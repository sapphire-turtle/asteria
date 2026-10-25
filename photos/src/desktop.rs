//! The desktop window: where the app's files are, and opening it. Desktop only; a phone
//! build starts the app its own way.

use std::error::Error;

use corundum_framework::gui::store::Store;
use corundum_framework::gui::window::{self, WindowOptions};
use corundum_framework::platform::dirs::AppDirs;

use crate::app::{AppPaths, PhotosApp};

/// The app's name, for its directories.
pub const APP_NAME: &str = "asteria-photos";

/// Where the app keeps `servers.toml` (its config directory) and the thumbnail cache (its
/// cache directory).
pub fn paths(dirs: &AppDirs) -> AppPaths {
    AppPaths {
        servers: dirs.config().join("servers.toml"),
        cache: Some(dirs.cache().to_path_buf()),
    }
}

/// Open the window and run the app until it closes.
pub fn run() -> Result<(), Box<dyn Error>> {
    let dirs = AppDirs::new(APP_NAME).ok_or("this system has no config or cache directory")?;
    let paths = paths(&dirs);
    let options = WindowOptions::new(APP_NAME).remember(Store::new(&dirs));
    window::run(options, move |cc| {
        Box::new(PhotosApp::new(paths, &cc.egui_ctx))
    })?;
    Ok(())
}
