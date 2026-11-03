#![cfg_attr(windows, windows_subsystem = "windows")]
//! `asteria-photos-desktop`: with no arguments it opens the window; with any, it is the
//! server's command line, the same as `asteria-photos` (headless). It also takes the files
//! dropped on the window to the app, and offers the server on this computer: the first run
//! when no server is registered, and Settings → Local server (spec §5.8). The phone build
//! has no use for either.

use std::error::Error;
use std::path::PathBuf;

use asteria_framework_client::profiles::Profiles;
use asteria_framework_desktop::{
    AboutPage, LocalServerPage, LocalServerSpec, UPDATE_PUBKEY, first_run_pages, service_spec,
    take_setup_request,
};
use asteria_framework_server::Tool;
use corundum_framework::egui;
use corundum_framework::gui::first_run::FirstRun;
use corundum_framework::gui::store::Store;
use corundum_framework::gui::tr;
use corundum_framework::gui::window::{self, WindowOptions};
use corundum_framework::package::{CliSpec, PackageSpec, Services};
use corundum_framework::platform::dirs::AppDirs;
use corundum_framework::platform::instance::Instance;
use corundum_framework::platform::launcher::LauncherSpec;
use corundum_framework::service::ServiceSpec;

use asteria_photos_gui::{APP_NAME, AppPaths, PhotosApp};

/// The server on this computer, for the first run and Settings → Local server.
static LOCAL: LocalServerSpec = LocalServerSpec {
    app: "asteria-photos",
    server: asteria_photos_server::spec,
    service,
    url: "http://127.0.0.1:7810",
    tools: &[Tool::Vipsthumbnail, Tool::Ffmpeg],
};

/// What the package is: the data directory's name (and the update feed's,
/// `asteria-photos-latest.json`), the command it puts on `PATH` and the server's service,
/// which an update stops and starts again.
static PACKAGE: PackageSpec = PackageSpec {
    app_name: APP_NAME,
    version: env!("CARGO_PKG_VERSION"),
    update_pubkey: UPDATE_PUBKEY,
    on_installed: None,
    on_uninstalling: None,
    before_apply: None,
    cli: Some(CliSpec {
        command: "asteria-photos",
    }),
    services: Services::new(&[service]),
};

/// The app's entry in the app menu.
const LAUNCHER: LauncherSpec = LauncherSpec {
    id: "asteria-photos",
    name: "Asteria Photos",
    description: "Your photo library, served from your own computer",
    category: "Graphics",
    icon_svg: include_bytes!("../../../docs/icons/asteria-photos.svg"),
    bundle_prefix: "org.sapphire-turtle",
};

/// The background service that runs `asteria-photos serve`.
fn service() -> ServiceSpec {
    service_spec("asteria-photos", "Asteria Photos server")
}

/// Whether the window opens on the first run: no server is registered yet, and the flow was
/// never finished on this device.
fn first_run_needed(servers: &Profiles, done: bool) -> bool {
    servers.servers.is_empty() && !done
}

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
    // Before `start`, so that what goes wrong in the package's install hooks is logged; not
    // for `serve`, whose server sets up its own logging.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if asteria_framework_desktop::entry(&args) != asteria_framework_desktop::Entry::Serve {
        init_logging();
    }
    let instance = asteria_framework_desktop::start(&LOCAL, &PACKAGE);
    run(instance)
}

/// What the window shows: the first run, or the app.
enum Stage {
    FirstRun(FirstRun),
    App(Box<PhotosApp>),
}

/// The window: the first run until it is finished, then the app with the files dropped on
/// it. "Set up a local server" in Settings brings a new first run back.
struct DesktopApp {
    paths: AppPaths,
    stage: Stage,
}

impl DesktopApp {
    /// The window's content, as it opens: the first run if [`first_run_needed`], else the
    /// app. A `servers.toml` that cannot be read goes to the app, which says so.
    fn new(paths: AppPaths, ctx: &egui::Context) -> DesktopApp {
        asteria_photos_gui::i18n::register(ctx);
        asteria_framework_desktop::register(ctx);
        let needed = Profiles::load(&paths.servers)
            .is_ok_and(|servers| first_run_needed(&servers, FirstRun::done(ctx)));
        let stage = if needed {
            Stage::FirstRun(first_run(&paths))
        } else {
            Stage::App(app(&paths, ctx))
        };
        DesktopApp { paths, stage }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        match &mut self.stage {
            Stage::FirstRun(flow) => {
                if flow.ui(ui) {
                    // Made now, it reads servers.toml afresh: the "This computer" the flow
                    // added is there and chosen, and with none it opens on adding a server.
                    self.stage = Stage::App(app(&self.paths, &ctx));
                    ctx.request_repaint();
                }
            }
            Stage::App(app) => {
                let dropped = take_drops(&ctx);
                if !dropped.is_empty() {
                    app.drop_files(dropped);
                }
                app.show(ui);
                drop_overlay(&ctx);
                if take_setup_request(&ctx) {
                    // The app goes first (its saver writing out what it holds); the flow is
                    // new, as a finished one has nothing left to show.
                    self.stage = Stage::FirstRun(first_run(&self.paths));
                    ctx.request_repaint();
                }
            }
        }
    }
}

/// A new first run, which registers a server it sets up in `paths.servers`.
fn first_run(paths: &AppPaths) -> FirstRun {
    FirstRun::new(
        "Asteria Photos",
        first_run_pages(&LOCAL, paths.servers.clone()),
    )
}

/// The app over `paths`, with Settings → Local server.
fn app(paths: &AppPaths, ctx: &egui::Context) -> Box<PhotosApp> {
    Box::new(PhotosApp::new(paths.clone(), ctx).with_extra_settings(vec![
        Box::new(LocalServerPage::new(&LOCAL)),
        Box::new(AboutPage::new(
            "Asteria Photos",
            CliSpec {
                command: "asteria-photos",
            },
            LAUNCHER,
            LOCAL.tools,
        )),
    ]))
}

impl corundum_framework::eframe::App for DesktopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut corundum_framework::eframe::Frame) {
        DesktopApp::ui(self, ui);
    }
}

/// Open the window and run the app until it closes. `instance` is the single-instance lock
/// `start` took, kept by the window.
fn run(instance: Instance) -> Result<(), Box<dyn Error>> {
    // Before anything opens the settings: on Windows they move from Roaming to Local.
    asteria_framework_client::migrate_dirs(APP_NAME)?;
    let dirs = AppDirs::new(APP_NAME).ok_or("this system has no config or cache directory")?;
    let paths = paths(&dirs);
    let options = WindowOptions::new(APP_NAME)
        .remember(Store::new(&dirs))
        .instance(instance)
        .launcher(LAUNCHER);
    window::run(options, move |cc| {
        Box::new(DesktopApp::new(paths, &cc.egui_ctx))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asteria_framework_client::profiles::Token;
    use asteria_photos_gui::app::Screen;

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
    fn the_package_is_the_apps_command_and_server() {
        assert_eq!(PACKAGE.app_name, APP_NAME);
        assert_eq!(PACKAGE.cli.map(|c| c.command), Some("asteria-photos"));
        assert_eq!(LAUNCHER.id, "asteria-photos");
        assert_eq!(LAUNCHER.name, "Asteria Photos");
    }

    #[test]
    fn the_first_run_is_needed_only_without_servers_and_unfinished() {
        let none = Profiles::default();
        assert!(first_run_needed(&none, false));
        assert!(!first_run_needed(&none, true));
        let mut one = Profiles::default();
        one.add("Home", "http://192.0.2.1:7810", Token::new("t"))
            .unwrap();
        assert!(!first_run_needed(&one, false));
        assert!(!first_run_needed(&one, true));
    }

    /// The app's files in `dir`: no cache, no uploads.
    fn temp_paths(dir: &tempfile::TempDir) -> AppPaths {
        AppPaths {
            servers: dir.path().join("config/servers.toml"),
            cache: None,
            data: None,
        }
    }

    /// One frame of the window in which `events` arrive.
    fn frame(
        ctx: &egui::Context,
        window: &mut DesktopApp,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| window.ui(ui));
        // There is no renderer to hand the texture uploads to.
        output.textures_delta.clear();
        output
    }

    /// Where the node labelled exactly `label` is, if the frame has one.
    fn find(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
        let update = output.platform_output.accesskit_update.as_ref()?;
        update.nodes.iter().find_map(|(_, node)| {
            let bounds = node.bounds()?;
            (node.label() == Some(label)).then(|| {
                egui::pos2(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                )
            })
        })
    }

    /// Draws the window until a frame shows `label`, then clicks it.
    fn click(ctx: &egui::Context, window: &mut DesktopApp, label: &str) {
        for _ in 0..10 {
            let output = frame(ctx, window, Vec::new());
            if let Some(at) = find(&output, label) {
                let button = |pressed| egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                frame(
                    ctx,
                    window,
                    vec![egui::Event::PointerMoved(at), button(true)],
                );
                frame(ctx, window, vec![button(false)]);
                return;
            }
        }
        panic!("no {label:?} to click");
    }

    /// Draws the window until it shows the app; the app.
    fn until_app<'a>(ctx: &egui::Context, window: &'a mut DesktopApp) -> &'a PhotosApp {
        for _ in 0..20 {
            if matches!(window.stage, Stage::App(_)) {
                break;
            }
            frame(ctx, window, Vec::new());
        }
        match &window.stage {
            Stage::App(app) => app,
            Stage::FirstRun(_) => panic!("the first run did not end"),
        }
    }

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        ctx
    }

    #[test]
    fn with_a_server_the_window_opens_on_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(&dir);
        let mut servers = Profiles::default();
        // Nothing listens on port 1: the app's connection fails at once.
        let id = servers
            .add("Home", "http://127.0.0.1:1", Token::new("t"))
            .unwrap();
        servers.set_active(Some(id)).unwrap();
        servers.save(&paths.servers).unwrap();
        let window = DesktopApp::new(paths, &context());
        assert!(matches!(window.stage, Stage::App(_)));
    }

    #[test]
    fn the_first_run_ends_on_the_app_ready_to_add_a_server() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context();
        let mut window = DesktopApp::new(temp_paths(&dir), &ctx);
        assert!(matches!(window.stage, Stage::FirstRun(_)));
        click(&ctx, &mut window, "Later");
        let app = until_app(&ctx, &mut window);
        assert!(app.profiles().servers.is_empty());
        assert_eq!(app.current_screen(), Some(Screen::Settings));
        assert!(FirstRun::done(&ctx));

        // "Set up a local server" in Settings brings a new first run back, from its start.
        asteria_framework_desktop::request_setup(&ctx);
        frame(&ctx, &mut window, Vec::new());
        assert!(matches!(window.stage, Stage::FirstRun(_)));
        let output = frame(&ctx, &mut window, Vec::new());
        assert!(find(&output, "Later").is_some(), "the flow starts over");
        click(&ctx, &mut window, "Another server");
        until_app(&ctx, &mut window);
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
