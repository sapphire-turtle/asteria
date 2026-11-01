//! The "Local server" settings page: the library and the address of the server on this
//! computer, its background service, its API keys, and the external tools.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::SystemTime;

use asteria_framework::Library;
use asteria_framework_client::settings::ExtraSettingsPage;
use asteria_framework_server::ServerConfig;
use corundum_framework::egui;
use corundum_framework::gui::first_run::FirstRun;
use corundum_framework::gui::keys::{KeysPage, LocalKeys};
use corundum_framework::gui::settings::ServiceRow;
use corundum_framework::gui::tr;

use crate::{LocalPaths, LocalServerSpec, tools_ui};

/// Where the "set up" request waits in the context's memory for the desktop to take it.
fn setup_request_id() -> egui::Id {
    // `unique` hashes its source: the same id every time.
    egui::Id::unique("asteria_desktop_setup_request")
}

/// Show the first-run flow again: forget that it was finished, and ask the desktop (through
/// [`take_setup_request`]) to bring it back now, whatever servers it has. The page's "Set up"
/// button does this.
pub fn request_setup(ctx: &egui::Context) {
    FirstRun::reset(ctx);
    ctx.data_mut(|d| d.insert_temp(setup_request_id(), true));
    ctx.request_repaint();
}

/// Whether the "Local server" page asked to set the server up since this was last asked: the
/// desktop then draws a new first-run flow in place of the app. Only the first call after
/// the request answers `true`.
///
/// [`FirstRun::reset`] alone would not do: a desktop that never showed the flow (it had a
/// server already) cannot tell the reset from the flow never having been finished.
pub fn take_setup_request(ctx: &egui::Context) -> bool {
    ctx.data_mut(|d| d.remove_temp::<bool>(setup_request_id()))
        .unwrap_or(false)
}

/// What the page found in the config file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageState {
    /// There is no config: no local server was set up.
    NotSetUp,
    /// The server serves `library` and listens on `listen`.
    Ready {
        library: PathBuf,
        listen: SocketAddr,
    },
    /// The config (or where it should be) could not be read, for the reason given.
    Unreadable(String),
}

/// The settings page for the server on this computer. It reads the config file again
/// whenever the file changes (a first run that set the server up, say).
pub struct LocalServerPage {
    spec: &'static LocalServerSpec,
    paths: Result<LocalPaths, String>,
    state: PageState,
    /// The config's modification time when last read; `None` when it was not there.
    read_at: Option<SystemTime>,
    /// The service's row, made when first drawn with a server set up.
    service: Option<ServiceRow>,
    /// The library's keys, or why they cannot be opened; made when first drawn.
    keys: Option<Result<KeysPage, String>>,
}

impl LocalServerPage {
    /// The page for `spec`'s app, over the default config file and the library's keys.
    pub fn new(spec: &'static LocalServerSpec) -> Self {
        let paths = LocalPaths::default_for(spec.app).map_err(|e| e.to_string());
        Self::over(spec, paths)
    }

    /// The page over the config and the keys at `paths`.
    pub fn with_paths(spec: &'static LocalServerSpec, paths: LocalPaths) -> Self {
        Self::over(spec, Ok(paths))
    }

    fn over(spec: &'static LocalServerSpec, paths: Result<LocalPaths, String>) -> Self {
        let mut page = LocalServerPage {
            spec,
            paths,
            state: PageState::NotSetUp,
            read_at: None,
            service: None,
            keys: None,
        };
        page.read();
        page
    }

    /// What the config file says, as last read.
    pub fn state(&self) -> &PageState {
        &self.state
    }

    /// The page's name in the settings list.
    pub fn label(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-desktop-page-label")
    }

    /// The config's modification time, `None` when it is not there.
    fn modified(&self) -> Option<SystemTime> {
        let paths = self.paths.as_ref().ok()?;
        std::fs::metadata(&paths.config).ok()?.modified().ok()
    }

    /// Read the config file again; the keys are opened again when next drawn.
    fn read(&mut self) {
        self.read_at = self.modified();
        self.keys = None;
        self.state = match &self.paths {
            Err(error) => PageState::Unreadable(error.clone()),
            Ok(paths) if !paths.config.exists() => PageState::NotSetUp,
            Ok(paths) => match ServerConfig::load(&paths.config) {
                Ok(cfg) => PageState::Ready {
                    library: cfg.library,
                    listen: cfg.listen,
                },
                Err(error) => PageState::Unreadable(error.to_string()),
            },
        };
    }

    /// The page over the keys of `library`, or why they cannot be opened.
    fn open_keys(&self, library: &std::path::Path) -> Result<KeysPage, String> {
        let paths = self.paths.as_ref().map_err(Clone::clone)?;
        let root = library.canonicalize().map_err(|e| e.to_string())?;
        let library = Library::open(&root).map_err(|e| e.to_string())?;
        let store = paths
            .open_keys(self.spec.app, &library.config().id)
            .map_err(|e| e.to_string())?;
        Ok(KeysPage::new(Box::new(LocalKeys::new(store))))
    }

    /// Draw the page.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if self.modified() != self.read_at {
            self.read();
        }
        ui.heading(tr!(ui, "asteria-desktop-page-label"));
        ui.add_space(8.0);
        match self.state.clone() {
            PageState::NotSetUp => {
                ui.label(tr!(ui, "asteria-desktop-page-not-set-up"));
                if ui.button(tr!(ui, "asteria-desktop-page-set-up")).clicked() {
                    request_setup(ui.ctx());
                }
            }
            PageState::Unreadable(error) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tr!(
                        ui,
                        "asteria-desktop-page-unreadable",
                        error = error.as_str()
                    ),
                );
            }
            PageState::Ready { library, listen } => self.ready_ui(ui, &library, listen),
        }
        ui.add_space(16.0);
        ui.heading(tr!(ui, "asteria-desktop-tools-title"));
        tools_ui(ui);
    }

    fn ready_ui(&mut self, ui: &mut egui::Ui, library: &std::path::Path, listen: SocketAddr) {
        egui::Grid::new("asteria_desktop_local_server")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label(tr!(ui, "asteria-desktop-page-library"));
                ui.label(library.display().to_string());
                ui.end_row();
                ui.label(tr!(ui, "asteria-desktop-page-listen"));
                ui.label(listen.to_string());
                ui.end_row();
                if let Ok(paths) = &self.paths {
                    ui.label(tr!(ui, "asteria-desktop-page-config"));
                    ui.label(paths.config.display().to_string());
                    ui.end_row();
                }
            });
        ui.label(egui::RichText::new(tr!(ui, "asteria-desktop-page-listen-note")).weak());

        ui.add_space(16.0);
        ui.heading(tr!(ui, "asteria-desktop-page-service"));
        let service = self.spec.service;
        self.service
            .get_or_insert_with(|| ServiceRow::new(service))
            .ui(ui);
        ui.label(egui::RichText::new(tr!(ui, "asteria-desktop-page-service-note")).weak());
        ui.code(format!("{} serve", self.spec.app));

        ui.add_space(16.0);
        ui.heading(tr!(ui, "asteria-desktop-page-keys"));
        if self.keys.is_none() {
            self.keys = Some(self.open_keys(library));
        }
        match self.keys.as_mut().expect("opened above") {
            Ok(keys) => keys.ui(ui),
            Err(error) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tr!(
                        ui,
                        "asteria-desktop-page-keys-failed",
                        error = error.as_str()
                    ),
                );
            }
        }
    }
}

impl ExtraSettingsPage for LocalServerPage {
    fn label(&self, ctx: &egui::Context) -> String {
        LocalServerPage::label(self, ctx)
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        LocalServerPage::ui(self, ui);
    }
}
