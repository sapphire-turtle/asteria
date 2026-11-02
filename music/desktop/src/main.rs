#![cfg_attr(windows, windows_subsystem = "windows")]
//! `asteria-music-desktop`: with no arguments it opens the window; with any, it is the
//! server's command line, the same as `asteria-music` (headless). It offers the server on
//! this computer: the first run when no server is registered, and Settings → Local server
//! (spec §5.8).

use std::error::Error;

use asteria_framework_client::profiles::Profiles;
use asteria_framework_desktop::{
    AboutPage, LocalServerPage, LocalServerSpec, first_run_pages, service_spec, take_setup_request,
};
use corundum_framework::egui;
use corundum_framework::gui::first_run::FirstRun;
use corundum_framework::gui::store::Store;
use corundum_framework::gui::window::{self, WindowOptions};
use corundum_framework::package::{CliSpec, PackageSpec, Services};
use corundum_framework::platform::dirs::AppDirs;
use corundum_framework::platform::instance::Instance;
use corundum_framework::platform::launcher::LauncherSpec;
use corundum_framework::service::ServiceSpec;

use asteria_music_gui::player::Player;
use asteria_music_gui::player::feed::HttpOpener;
use asteria_music_gui::player::output::CpalOutputs;
use asteria_music_gui::{APP_NAME, AppPaths, MusicApp};

/// The server on this computer, for the first run and Settings → Local server.
static LOCAL: LocalServerSpec = LocalServerSpec {
    app: "asteria-music",
    server: asteria_music_server::spec,
    service,
    url: "http://127.0.0.1:7811",
};

/// What the Velopack package is: the data directory's name, the command it puts on `PATH`
/// and the server's service, which an update stops and starts again.
static PACKAGE: PackageSpec = PackageSpec {
    app_name: APP_NAME,
    on_installed: None,
    on_uninstalling: None,
    before_apply: None,
    cli: Some(CliSpec {
        command: "asteria-music",
    }),
    services: Services::new(&[service]),
};

/// The app's entry in the app menu.
const LAUNCHER: LauncherSpec = LauncherSpec {
    id: "asteria-music",
    name: "Asteria Music",
    description: "Your music library, served from your own computer",
    category: "AudioVideo",
    icon_svg: include_bytes!("../../../docs/icons/asteria-music.svg"),
    bundle_prefix: "net.sapphire-turtle",
};

/// The background service that runs `asteria-music serve`.
fn service() -> ServiceSpec {
    service_spec("asteria-music", "Asteria Music server")
}

/// Whether the window opens on the first run: no server is registered yet, and the flow was
/// never finished on this device.
fn first_run_needed(servers: &Profiles, done: bool) -> bool {
    servers.servers.is_empty() && !done
}

/// Where the app keeps `servers.toml` and `playback.toml` (its config directory) and the
/// cover cache and the streamed tracks (its cache directory).
fn paths(dirs: &AppDirs) -> AppPaths {
    AppPaths {
        servers: dirs.config().join("servers.toml"),
        playback: dirs.config().join("playback.toml"),
        cache: Some(dirs.cache().to_path_buf()),
    }
}

/// Sends the app's log to stderr: warnings and errors unless `RUST_LOG` says otherwise
/// (`RUST_LOG=asteria_music=debug`, say). Nothing the app logs carries a token: tokens only
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
    // Before `start`, so that what goes wrong in Velopack's hooks is logged; not for `serve`,
    // whose server sets up its own logging.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if asteria_framework_desktop::entry(&args) != asteria_framework_desktop::Entry::Serve {
        init_logging();
    }
    let instance = asteria_framework_desktop::start(&LOCAL, &PACKAGE);
    run(instance)
}

/// Starts the playback thread for an app made in `ctx`.
type SpawnPlayer = Box<dyn Fn(&egui::Context) -> Player>;

/// What the window shows: the first run, or the app.
enum Stage {
    FirstRun(FirstRun),
    App(Box<MusicApp>),
}

/// The window: the first run until it is finished, then the app. "Set up a local server" in
/// Settings brings a new first run back.
///
/// Each app is made with a player of its own and stops it when it goes (dropping a
/// [`Player`] stops playback and joins its thread): the first run plays nothing, and a
/// player kept across apps would carry the old app's queue into the new one.
struct DesktopApp {
    paths: AppPaths,
    spawn_player: SpawnPlayer,
    stage: Stage,
}

impl DesktopApp {
    /// The window's content, as it opens: the first run if [`first_run_needed`], else the
    /// app. A `servers.toml` that cannot be read goes to the app, which says so.
    fn new(paths: AppPaths, ctx: &egui::Context, spawn_player: SpawnPlayer) -> DesktopApp {
        asteria_music_gui::i18n::register(ctx);
        asteria_framework_desktop::register(ctx);
        let needed = Profiles::load(&paths.servers)
            .is_ok_and(|servers| first_run_needed(&servers, FirstRun::done(ctx)));
        let stage = if needed {
            Stage::FirstRun(first_run(&paths))
        } else {
            Stage::App(app(&paths, ctx, &spawn_player))
        };
        DesktopApp {
            paths,
            spawn_player,
            stage,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        match &mut self.stage {
            Stage::FirstRun(flow) => {
                if flow.ui(ui) {
                    // Made now, it reads servers.toml afresh: the "This computer" the flow
                    // added is there and chosen, and with none it opens on adding a server.
                    self.stage = Stage::App(app(&self.paths, &ctx, &self.spawn_player));
                    ctx.request_repaint();
                }
            }
            Stage::App(app) => {
                app.show(ui);
                if take_setup_request(&ctx) {
                    // The app goes first (its player stopped, its saver writing out what it
                    // holds); the flow is new, as a finished one has nothing left to show.
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
        "Asteria Music",
        first_run_pages(&LOCAL, paths.servers.clone()),
    )
}

/// The app over `paths`, with a new player and Settings → Local server.
fn app(paths: &AppPaths, ctx: &egui::Context, spawn_player: &SpawnPlayer) -> Box<MusicApp> {
    Box::new(
        MusicApp::new(paths.clone(), ctx, spawn_player(ctx)).with_extra_settings(vec![
            Box::new(LocalServerPage::new(&LOCAL)),
            Box::new(AboutPage::new(
                "Asteria Music",
                CliSpec {
                    command: "asteria-music",
                },
                LAUNCHER,
            )),
        ]),
    )
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
        let spawn_player: SpawnPlayer = Box::new(|ctx| {
            Player::spawn(
                Box::new(HttpOpener::new()),
                Box::new(CpalOutputs),
                ctx.clone(),
            )
            // The window cannot report an app that fails to start; this is the OS refusing
            // a thread.
            .expect("starting the playback thread failed")
        });
        Box::new(DesktopApp::new(paths, &cc.egui_ctx, spawn_player))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use asteria_framework_client::profiles::Token;
    use asteria_music_gui::app::Screen;
    use asteria_music_gui::player::fakes::{FakeOpener, FakeOutputs};

    /// The app's files in `dir`: no cache.
    fn temp_paths(dir: &tempfile::TempDir) -> AppPaths {
        AppPaths {
            servers: dir.path().join("config/servers.toml"),
            playback: dir.path().join("config/playback.toml"),
            cache: None,
        }
    }

    /// Players on the fakes, counted as they are started.
    fn fake_players() -> (SpawnPlayer, Rc<Cell<usize>>) {
        let started = Rc::new(Cell::new(0));
        let count = started.clone();
        let spawn: SpawnPlayer = Box::new(move |ctx| {
            count.set(count.get() + 1);
            Player::spawn(
                Box::new(FakeOpener::new()),
                Box::new(FakeOutputs::new()),
                ctx.clone(),
            )
            .unwrap()
        });
        (spawn, started)
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
    fn until_app<'a>(ctx: &egui::Context, window: &'a mut DesktopApp) -> &'a MusicApp {
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
    fn the_package_is_the_apps_command_and_server() {
        assert_eq!(PACKAGE.app_name, APP_NAME);
        assert_eq!(PACKAGE.cli.map(|c| c.command), Some("asteria-music"));
        assert_eq!(LAUNCHER.id, "asteria-music");
        assert_eq!(LAUNCHER.name, "Asteria Music");
    }

    #[test]
    fn the_first_run_is_needed_only_without_servers_and_unfinished() {
        let none = Profiles::default();
        assert!(first_run_needed(&none, false));
        assert!(!first_run_needed(&none, true));
        let mut one = Profiles::default();
        one.add("Home", "http://192.0.2.1:7811", Token::new("t"))
            .unwrap();
        assert!(!first_run_needed(&one, false));
        assert!(!first_run_needed(&one, true));
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
        let (spawn, started) = fake_players();
        let window = DesktopApp::new(paths, &context(), spawn);
        assert!(matches!(window.stage, Stage::App(_)));
        assert_eq!(started.get(), 1);
    }

    #[test]
    fn the_first_run_ends_on_the_app_ready_to_add_a_server() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context();
        let (spawn, started) = fake_players();
        let mut window = DesktopApp::new(temp_paths(&dir), &ctx, spawn);
        assert!(matches!(window.stage, Stage::FirstRun(_)));
        assert_eq!(started.get(), 0, "the first run plays nothing");
        click(&ctx, &mut window, "Later");
        let app = until_app(&ctx, &mut window);
        assert!(app.profiles().servers.is_empty());
        assert_eq!(app.current_screen(), Some(Screen::Settings));
        assert_eq!(started.get(), 1);

        // "Set up a local server" in Settings brings a new first run back, from its start;
        // the app and its player go.
        asteria_framework_desktop::request_setup(&ctx);
        frame(&ctx, &mut window, Vec::new());
        assert!(matches!(window.stage, Stage::FirstRun(_)));
        let output = frame(&ctx, &mut window, Vec::new());
        assert!(find(&output, "Later").is_some(), "the flow starts over");
        click(&ctx, &mut window, "Another server");
        until_app(&ctx, &mut window);
        assert_eq!(started.get(), 2, "the new app has a player of its own");
    }
}
