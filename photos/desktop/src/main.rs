//! `asteria-photos-desktop`: with no arguments it opens the window; with any, it is the
//! server's command line, the same as `asteria-photos` (headless). It also takes the files
//! dropped on the window to the app, which the phone build has no use for.

use std::error::Error;
use std::path::PathBuf;

use corundum_framework::egui;
use corundum_framework::gui::store::Store;
use corundum_framework::gui::tr;
use corundum_framework::gui::window::{self, WindowOptions};
use corundum_framework::platform::dirs::AppDirs;

use asteria_photos_gui::{APP_NAME, AppPaths, PhotosApp};

/// Where the app keeps `servers.toml` (its config directory), the thumbnail cache (its cache
/// directory) and the upload queues (its data directory).
fn paths(dirs: &AppDirs) -> AppPaths {
    AppPaths {
        servers: dirs.config().join("servers.toml"),
        cache: Some(dirs.cache().to_path_buf()),
        data: Some(dirs.data().to_path_buf()),
    }
}

/// The paths of the files dropped on the window this frame. The first thing the app does with
/// the OS's drag and drop; nothing else in the app depends on it.
fn take_drops(ctx: &egui::Context) -> Vec<PathBuf> {
    ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .map(|f| f.path().to_path_buf())
            .collect()
    })
}

/// While files are held over the window, a veil over all of it that says where they go.
fn drop_overlay(ctx: &egui::Context) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        // `unique` hashes its source: the same id every frame.
        egui::Id::unique("asteria-photos-drop-overlay"),
    ));
    painter.rect_filled(
        screen,
        0.0,
        ctx.global_style()
            .visuals
            .extreme_bg_color
            .gamma_multiply(0.85),
    );
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        tr!(ctx, "asteria-photos-drop-overlay"),
        egui::FontId::proportional(28.0),
        ctx.global_style().visuals.strong_text_color(),
    );
}

/// Sends the app's log to stderr: warnings and errors unless `RUST_LOG` says otherwise
/// (`RUST_LOG=asteria_photos=debug`, say). Nothing the app logs carries a token: tokens only
/// travel in the `Authorization` header, and their `Debug` output is masked.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // Fails only if a subscriber is already installed, which then keeps logging.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args_os().nth(1).is_some() {
        asteria_framework_server::cli::main(asteria_photos_server::spec())
    }
    run()
}

/// The app in the window, with the files dropped on it.
struct DesktopApp(PhotosApp);

impl corundum_framework::eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut corundum_framework::eframe::Frame) {
        let dropped = take_drops(ui.ctx());
        if !dropped.is_empty() {
            self.0.drop_files(dropped);
        }
        self.0.show(ui);
        drop_overlay(ui.ctx());
    }
}

/// Open the window and run the app until it closes.
fn run() -> Result<(), Box<dyn Error>> {
    init_logging();
    // Before anything opens the settings: on Windows they move from Roaming to Local.
    asteria_framework_client::migrate_dirs(APP_NAME)?;
    let dirs = AppDirs::new(APP_NAME).ok_or("this system has no config or cache directory")?;
    let paths = paths(&dirs);
    let options = WindowOptions::new(APP_NAME).remember(Store::new(&dirs));
    window::run(options, move |cc| {
        Box::new(DesktopApp(PhotosApp::new(paths, &cc.egui_ctx)))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file dropped on the window, as the OS hands it over.
    #[derive(Debug)]
    struct Dropped(PathBuf);

    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Err("not read".into())
        }
    }

    #[test]
    fn dropped_files_are_taken_from_the_frame() {
        let ctx = egui::Context::default();
        let path = PathBuf::from("trip/a.jpg");
        let input = egui::RawInput {
            dropped_files: vec![std::sync::Arc::new(Dropped(path.clone()))],
            ..Default::default()
        };
        let mut taken = Vec::new();
        let mut output = ctx.run_ui(input, |ui| taken = take_drops(ui.ctx()));
        // There is no renderer to hand the texture uploads to.
        output.textures_delta.clear();
        assert_eq!(taken, [path]);
    }
}
