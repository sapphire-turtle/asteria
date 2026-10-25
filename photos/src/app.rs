//! [`PhotosApp`]: the registered servers, the connection to the one in use, and the screens.
//!
//! Everything tied to a server lives in a [`Connection`]: its [`Session`] (requests, images,
//! cache) and its [`BrowseState`] (the folder shown, its pages, the selection, the polling).
//! Switching servers drops the connection whole and opens a new one at the library's root,
//! so nothing the old server sends can reach the new screen.
//!
//! The servers are kept in `servers.toml`. It is read once, before the first frame; every
//! change is written by a background thread, so the UI thread does no disk I/O.

use std::path::PathBuf;

use asteria_framework_api::ItemId;
use asteria_framework_client::profiles::{ProfileId, Profiles};
use asteria_framework_client::profiles::{ServerProfile, Token};
use asteria_framework_client::session::SessionState;
use asteria_framework_client::settings::servers::{
    ServersChange, ServersPage, error_detail, error_text,
};
use corundum_framework::egui;
use corundum_framework::gui::settings::{ListItem, SettingsLayout, SettingsSection};
use corundum_framework::gui::theme::{Swatch, Tone};
use corundum_framework::gui::{COMPACT_WIDTH, Destination, Shell, ShellApp, Side, icons, tr};

pub use crate::connection::Connection;
use crate::saver::Saver;

/// The screens: the library on the rail, Settings at its foot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Library,
    Settings,
}

/// The pages of the Settings screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Servers,
}

/// Where the app keeps its files.
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// The registered servers and the one in use.
    pub servers: PathBuf,
    /// The root of the thumbnail cache (one directory per library under it); `None` keeps
    /// no cache on disk.
    pub cache: Option<PathBuf>,
}

/// The app. Hold one; call [`show`](Self::show) every frame with the window's root `Ui`.
pub struct PhotosApp {
    shell: Shell<Screen>,
    state: State,
}

/// Everything but the shell, which draws it.
struct State {
    ctx: egui::Context,
    paths: AppPaths,
    profiles: Profiles,
    connection: Option<Connection>,
    servers_page: ServersPage,
    settings: SettingsLayout<Page>,
    /// `None` when `servers.toml` could not be read (saving would overwrite it) or the save
    /// thread could not start; [`State::save_blocked`] says why.
    saver: Option<Saver>,
    save_blocked: Option<String>,
    /// A screen to open after this frame (from the status bar's menu, say).
    open_screen: Option<Screen>,
    /// Whether the shell lays out for a phone this frame (no status bar).
    compact: bool,
}

impl PhotosApp {
    /// Reads `servers.toml` (this is the one disk read on the UI thread, before the first
    /// frame) and connects to the server used last. With no server registered, it opens on
    /// Settings, ready to add one.
    pub fn new(paths: AppPaths, ctx: &egui::Context) -> PhotosApp {
        crate::i18n::register(ctx);
        // Ruling R11: read on the UI thread, but before the first frame; the file is tiny.
        let (profiles, load_error) = match Profiles::load(&paths.servers) {
            Ok(p) => (p, None),
            Err(e) => {
                tracing::warn!(path = %paths.servers.display(), error = %e, "reading servers.toml failed");
                (Profiles::default(), Some(e.to_string()))
            }
        };
        let mut servers_page = ServersPage::new(&profiles, asteria_photos_api::client_info());
        let (saver, save_blocked) = match load_error {
            Some(e) => (
                None,
                Some(tr!(ctx, "asteria-photos-load-failed", reason = e)),
            ),
            None => match Saver::spawn(paths.servers.clone(), ctx.clone()) {
                Ok(s) => (Some(s), None),
                Err(e) => (
                    None,
                    Some(tr!(
                        ctx,
                        "asteria-photos-save-failed",
                        reason = e.to_string()
                    )),
                ),
            },
        };
        if let Some(why) = &save_blocked {
            servers_page.show_error(why.clone());
        }
        let shell = Shell::new(vec![Destination::new(
            Screen::Library,
            icons::images(),
            "asteria-photos-screen-library",
        )])
        .with_footer(vec![Destination::new(
            Screen::Settings,
            icons::settings(),
            "asteria-photos-screen-settings",
        )])
        .with_status_bar();
        let mut state = State {
            ctx: ctx.clone(),
            paths,
            profiles,
            connection: None,
            servers_page,
            settings: SettingsLayout::new(),
            saver,
            save_blocked,
            open_screen: None,
            compact: false,
        };
        state.connect(false);
        if state.profiles.servers.is_empty() {
            state.open_servers_settings();
            state.servers_page.begin_add();
        }
        let mut app = PhotosApp { shell, state };
        app.apply_open_screen();
        app
    }

    /// Draw one frame into the window's root `ui`, after taking in what arrived.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.state.poll();
        self.state.compact = is_compact(ui);
        self.shell.show(ui, &mut self.state);
        self.state.apply_servers_changes();
        self.apply_open_screen();
    }

    fn apply_open_screen(&mut self) {
        if let Some(screen) = self.state.open_screen.take() {
            self.shell.open(screen);
            self.state.ctx.request_repaint();
        }
    }

    /// The registered servers.
    pub fn profiles(&self) -> &Profiles {
        &self.state.profiles
    }

    /// The server in use.
    pub fn active_profile(&self) -> Option<ProfileId> {
        self.state.profiles.active().map(|p| p.id)
    }

    /// The connection to the server in use, if one is chosen.
    pub fn connection(&self) -> Option<&Connection> {
        self.state.connection.as_ref()
    }

    /// The screen shown.
    pub fn current_screen(&self) -> Option<Screen> {
        self.shell.current()
    }

    /// Use the server `id` from now on: saved as the one in use, connected afresh, at the
    /// library's root. Choosing the server in use again only reconnects if it had failed.
    pub fn switch_to(&mut self, id: ProfileId) {
        self.state.switch_to(id);
    }

    /// Show the folder `path` (library-relative, `""` the root) of the server in use.
    pub fn open_folder(&mut self, path: &str) {
        if let Some(c) = &mut self.state.connection {
            c.browse.open(&mut c.session, path);
        }
    }

    /// Ask the server for `library.info` now, rather than at the next scheduled check.
    pub fn refresh_library(&mut self) {
        if let Some(c) = &mut self.state.connection {
            c.browse.refresh_library(&mut c.session);
        }
    }

    /// The item last opened (double-clicked) in the grid.
    pub fn opened_item(&self) -> Option<&ItemId> {
        self.state.connection.as_ref()?.browse.opened()
    }

    /// The Settings page of the servers, as the user works it.
    pub fn servers_page_mut(&mut self) -> &mut ServersPage {
        &mut self.state.servers_page
    }
}

impl State {
    /// Opens a connection to the active server, dropping the one there was.
    fn connect(&mut self, reopened: bool) {
        // The old session goes first: its pending results die with it.
        self.connection = None;
        self.connection = self.profiles.active().map(|profile| {
            Connection::open(profile, &self.ctx, self.paths.cache.clone(), reopened)
        });
    }

    fn switch_to(&mut self, id: ProfileId) {
        let same = self.profiles.active().map(|p| p.id) == Some(id);
        let healthy = self
            .connection
            .as_ref()
            .is_some_and(|c| !matches!(c.session.state(), SessionState::Failed(_)));
        if same && healthy {
            return;
        }
        if let Err(e) = self.profiles.set_active(Some(id)) {
            tracing::warn!(error = %e, "switching to a server that is not registered");
            return;
        }
        // The page first: `set_profiles` clears its error, which `save` may set.
        self.servers_page.set_profiles(&self.profiles);
        self.save();
        self.connect(false);
        self.ctx.request_repaint();
    }

    fn save(&mut self) {
        match &self.saver {
            Some(saver) => saver.save(&self.profiles),
            None => {
                if let Some(why) = &self.save_blocked {
                    self.servers_page.show_error(why.clone());
                }
            }
        }
    }

    /// Takes in what arrived: save failures, and the session's replies.
    fn poll(&mut self) {
        if let Some(saver) = &self.saver {
            for e in saver.errors() {
                let why = tr!(self.ctx, "asteria-photos-save-failed", reason = e);
                self.servers_page.show_error(why);
            }
        }
        if let Some(c) = &mut self.connection {
            c.poll(&self.ctx);
        }
        // The server now serves another library: start again with it, at the root.
        if self
            .connection
            .as_ref()
            .is_some_and(Connection::library_changed)
        {
            tracing::info!("the server's library changed; reconnecting");
            self.connect(true);
        }
    }

    /// Applies what the user did on the servers page, saves it, and reconnects when the
    /// server in use changed.
    fn apply_servers_changes(&mut self) {
        let Some(change) = self.servers_page.take_change() else {
            return;
        };
        let before = endpoint(self.profiles.active());
        let result = match change {
            ServersChange::Added { name, url, token } => {
                self.profiles.add(name, url.as_str(), token).and_then(|id| {
                    // The first server is the one used.
                    if self.profiles.active().is_none() {
                        self.profiles.set_active(Some(id))?;
                    }
                    Ok(())
                })
            }
            ServersChange::Updated(p) => {
                self.profiles.update(p.id, p.name, p.url.as_str(), p.token)
            }
            ServersChange::Removed(id) => {
                self.profiles.remove(id);
                Ok(())
            }
            ServersChange::Activate(id) => {
                // Saves and reconnects as a switch from the status bar does.
                self.switch_to(id);
                return;
            }
        };
        self.servers_page.set_profiles(&self.profiles);
        if let Err(e) = result {
            let why = tr!(
                self.ctx,
                "asteria-photos-save-failed",
                reason = e.to_string()
            );
            self.servers_page.show_error(why);
            return;
        }
        self.save();
        // Another server, or the same one with a new address or token; a new name alone
        // keeps the connection.
        if before != endpoint(self.profiles.active()) {
            self.connect(false);
        }
        self.ctx.request_repaint();
    }

    fn open_servers_settings(&mut self) {
        self.settings.open(Some(Page::Servers));
        self.open_screen = Some(Screen::Settings);
    }

    fn library_ui(&mut self, ui: &mut egui::Ui) {
        let Some(c) = &mut self.connection else {
            egui::CentralPanel::default().show(ui, |ui| {
                let hint = if self.profiles.servers.is_empty() {
                    tr!(ui, "asteria-photos-library-no-servers")
                } else {
                    tr!(ui, "asteria-photos-library-no-active")
                };
                ui.label(hint);
                if ui
                    .button(tr!(ui, "asteria-photos-library-open-servers"))
                    .clicked()
                {
                    self.open_servers_settings();
                }
            });
            return;
        };
        let name = self
            .profiles
            .active()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        match c.session.state() {
            SessionState::Failed(e) => {
                let reason = error_text(ui.ctx(), e);
                let mut retry = false;
                let mut manage = false;
                let detail = error_detail(e);
                egui::CentralPanel::default().show(ui, |ui| {
                    let label = ui.colored_label(
                        ui.visuals().error_fg_color,
                        tr!(
                            ui,
                            "asteria-photos-library-connect-failed",
                            name = name.as_str(),
                            reason = reason
                        ),
                    );
                    if let Some(detail) = detail {
                        label.on_hover_text(detail);
                    }
                    ui.horizontal(|ui| {
                        retry = ui.button(tr!(ui, "asteria-photos-library-retry")).clicked();
                        manage = ui
                            .button(tr!(ui, "asteria-photos-library-open-servers"))
                            .clicked();
                    });
                });
                if retry {
                    self.connect(false);
                }
                if manage {
                    self.open_servers_settings();
                }
            }
            SessionState::Connecting => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak(tr!(
                            ui,
                            "asteria-photos-library-connecting",
                            name = name.as_str()
                        ));
                    });
                });
            }
            SessionState::Ready { .. } => {
                c.browse.ui(ui, &mut c.session);
            }
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let items = [ListItem::Entry {
            page: Page::Servers,
            label: self.servers_page.label(ui.ctx()),
            dirty: self.servers_page.dirty(),
        }];
        let page = &mut self.servers_page;
        self.settings.ui(ui, &items, |ui, open| match open {
            Page::Servers => page.ui(ui),
        });
    }

    /// The server in use and its state, with a menu of the servers to switch to. `compact`
    /// shows only the state's icon (the bottom bar has little room).
    fn switcher_ui(&mut self, ui: &mut egui::Ui, compact: bool) {
        let (icon, swatch, status) = match self.connection.as_ref().map(|c| c.session.state()) {
            None => (
                icons::cloud_off(),
                Swatch::Neutral,
                tr!(ui, "asteria-photos-status-none"),
            ),
            Some(SessionState::Connecting) => (
                icons::refresh_cw(),
                Swatch::Yellow,
                tr!(ui, "asteria-photos-status-connecting"),
            ),
            Some(SessionState::Ready { .. }) => (
                icons::circle_check(),
                Swatch::Green,
                tr!(ui, "asteria-photos-status-connected"),
            ),
            Some(SessionState::Failed(e)) => {
                let reason = error_text(ui.ctx(), e);
                (
                    icons::circle_x(),
                    Swatch::Red,
                    tr!(ui, "asteria-photos-status-failed", reason = reason),
                )
            }
        };
        let dot = icons::image_as(ui, icon, 14.0, Tone::Text, swatch);
        let name = match self.profiles.active() {
            Some(p) => p.name.clone(),
            None => tr!(ui, "asteria-photos-status-none"),
        };
        let button = if compact {
            egui::Button::image(dot)
        } else {
            egui::Button::image_and_text(dot, name.as_str())
        };
        let active = self.profiles.active().map(|p| p.id);
        let servers: Vec<(ProfileId, String)> = self
            .profiles
            .servers
            .iter()
            .map(|p| (p.id, p.name.clone()))
            .collect();
        let menu =
            egui::containers::menu::MenuButton::from_button(button.frame(false)).ui(ui, |ui| {
                ui.strong(name.as_str());
                ui.weak(status.as_str());
                ui.separator();
                let mut picked = None;
                for (id, server) in &servers {
                    if ui.radio(active == Some(*id), server.as_str()).clicked() {
                        picked = Some(*id);
                        ui.close();
                    }
                }
                if !servers.is_empty() {
                    ui.separator();
                }
                let manage = ui.button(tr!(ui, "asteria-photos-status-manage")).clicked();
                if manage {
                    ui.close();
                }
                (picked, manage)
            });
        menu.0.on_hover_text(tr!(
            ui,
            "asteria-photos-status-hover",
            name = name.as_str(),
            status = status.as_str()
        ));
        if let Some(inner) = menu.1 {
            let (picked, manage) = inner.inner;
            if let Some(id) = picked {
                self.switch_to(id);
            }
            if manage {
                self.open_servers_settings();
            }
        }
    }
}

/// What a connection depends on: the server, its address and its token (not its name).
fn endpoint(p: Option<&ServerProfile>) -> Option<(ProfileId, String, Token)> {
    p.map(|p| (p.id, p.url.as_str().to_string(), p.token.clone()))
}

/// Whether the shell lays this frame out as a phone's (a bottom bar, no status bar), by
/// corundum's own rule: the root `ui`'s width less the system's bars and notches.
fn is_compact(ui: &egui::Ui) -> bool {
    let safe = ui.ctx().content_rect();
    let full = ui.max_rect();
    let left = (safe.left() - full.left()).max(0.0);
    let right = (full.right() - safe.right()).max(0.0);
    full.width() - left - right < COMPACT_WIDTH
}

impl ShellApp<Screen> for State {
    fn screen_ui(&mut self, ui: &mut egui::Ui, screen: Screen, _side: &mut Side) {
        match screen {
            Screen::Library => self.library_ui(ui),
            Screen::Settings => self.settings_ui(ui),
        }
    }

    /// Wide, the status bar holds the switcher ([`status_ui`](Self::status_ui)); narrow
    /// there is no status bar, so it sits at the bottom bar's end.
    fn foot_ui(&mut self, ui: &mut egui::Ui) -> Option<Screen> {
        if self.compact {
            self.switcher_ui(ui, true);
        }
        None
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        self.switcher_ui(ui, false);
    }
}

impl corundum_framework::eframe::App for PhotosApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut corundum_framework::eframe::Frame) {
        self.show(ui);
    }
}
