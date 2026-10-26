//! [`PhotosApp`]: the registered servers, the connection to the one in use, and the screens.
//!
//! Everything tied to a server lives in a [`Connection`]: its [`Session`] (requests, images,
//! cache) and its [`BrowseState`] (the folder shown, its pages, the selection, the polling).
//! Switching servers drops the connection whole and opens a new one at the library's root,
//! so nothing the old server sends can reach the new screen.
//!
//! The selected item's details are in corundum's side pane (a right column when wide), and
//! an item opened from the grid fills the window ([`crate::viewer`]) until Esc.
//!
//! The servers are kept in `servers.toml`. It is read once, before the first frame; every
//! change is written by a background thread, so the UI thread does no disk I/O.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use asteria_framework_api::ItemId;
use asteria_framework_client::errors::{error_detail, error_text};
use asteria_framework_client::profiles::{ProfileId, Profiles};
use asteria_framework_client::profiles::{ServerProfile, Token};
use asteria_framework_client::session::{SessionDirs, SessionState};
use asteria_framework_client::settings::servers::{ServersChange, ServersPage};
use corundum_framework::egui;
use corundum_framework::gui::settings::{ListItem, SettingsLayout, SettingsSection};
use corundum_framework::gui::theme::{Swatch, Tone};
use corundum_framework::gui::{COMPACT_WIDTH, Destination, Shell, ShellApp, Side, icons, tr};

pub use crate::connection::Connection;
use crate::detail::{DetailView, Edit};
use crate::saver::Saver;
use crate::viewer;

/// The screens: the library on the rail, Settings at its foot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Library,
    Uploads,
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
    /// The root of the app's data; the upload queues are kept under it. `None` turns uploads
    /// off.
    pub data: Option<PathBuf>,
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
    /// Open the side pane on the next frame of the library (the viewer's "Details").
    open_details: bool,
    /// Something to tell the user on the status bar, and since when.
    notice: Option<(String, Instant)>,
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
        let shell = Shell::new(vec![
            Destination::new(
                Screen::Library,
                icons::images(),
                "asteria-photos-screen-library",
            ),
            Destination::new(
                Screen::Uploads,
                icons::upload(),
                "asteria-photos-screen-uploads",
            ),
        ])
        .with_footer(vec![Destination::new(
            Screen::Settings,
            icons::settings(),
            "asteria-photos-screen-settings",
        )])
        .with_status_bar()
        .with_side_pane();
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
            open_details: false,
            notice: None,
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
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        {
            let dropped = crate::desktop::take_drops(ui.ctx());
            if !dropped.is_empty() {
                self.drop_files(dropped);
            }
            crate::desktop::drop_overlay(ui.ctx());
        }
        self.state.compact = is_compact(ui);
        if self.shell.current() == Some(Screen::Library) && self.state.viewer_open() {
            self.state.viewer_ui(ui);
        } else {
            self.shell.show(ui, &mut self.state);
        }
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
    /// library's root. Choosing the server in use again only reconnects if it had failed or
    /// stopped answering ([`Session::degraded`](asteria_framework_client::session::Session::degraded)).
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

    /// The item shown full screen (opened from the grid by a double-click or Enter).
    pub fn opened_item(&self) -> Option<&ItemId> {
        self.state.connection.as_ref()?.browse.opened()
    }

    /// Select `id` in the listing shown, as a click on it does.
    pub fn select(&mut self, id: &ItemId) {
        if let Some(c) = &mut self.state.connection {
            c.browse.select(id);
        }
    }

    /// Send `edit` of the selected item, as the detail pane does. Returns whether it was
    /// sent (see [`BrowseState::edit`](crate::browse::BrowseState::edit)).
    pub fn edit(&mut self, edit: Edit) -> bool {
        match &mut self.state.connection {
            Some(c) => c.browse.edit(&mut c.session, edit),
            None => false,
        }
    }

    /// What the detail pane shows for the selected item.
    pub fn detail(&self) -> Option<DetailView<'_>> {
        self.state.connection.as_ref()?.browse.detail()
    }

    /// Queue `paths` (files and folders) for upload to the server's Inbox and open the Uploads
    /// screen. Not connected, nothing is queued and the status bar says so.
    pub fn drop_files(&mut self, paths: Vec<PathBuf>) {
        let uploads = self
            .state
            .connection
            .as_ref()
            .and_then(|c| c.session.uploads());
        match uploads {
            Some(uploads) => {
                uploads.drop_paths(paths);
                self.state.notice = None;
                self.state.open_screen = Some(Screen::Uploads);
                self.apply_open_screen();
            }
            None => {
                let text = tr!(self.state.ctx, "asteria-photos-drop-refused");
                self.state.notice = Some((text, Instant::now()));
                self.state.ctx.request_repaint();
            }
        }
    }

    /// What the status bar says about a refused drop, while it does.
    pub fn notice(&self) -> Option<&str> {
        let (text, since) = self.state.notice.as_ref()?;
        (since.elapsed() < NOTICE_FOR).then_some(text.as_str())
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
            let dirs = SessionDirs {
                cache: self.paths.cache.clone(),
                data: self.paths.data.clone(),
            };
            Connection::open(profile, &self.ctx, dirs, reopened)
        });
    }

    fn switch_to(&mut self, id: ProfileId) {
        let same = self.profiles.active().map(|p| p.id) == Some(id);
        let healthy = self.connection.as_ref().is_some_and(|c| {
            !matches!(c.session.state(), SessionState::Failed(_)) && c.session.degraded().is_none()
        });
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

    /// Whether an item is shown full screen, on a connection that works.
    fn viewer_open(&self) -> bool {
        self.connection.as_ref().is_some_and(|c| {
            matches!(c.session.state(), SessionState::Ready { .. }) && c.browse.opened().is_some()
        })
    }

    fn viewer_ui(&mut self, ui: &mut egui::Ui) {
        let Some(c) = &mut self.connection else {
            return;
        };
        if viewer::ui(ui, &mut c.browse, &mut c.session) == viewer::Asked::Details {
            self.open_details = true;
        }
    }

    fn library_ui(&mut self, ui: &mut egui::Ui, side: &mut Side) {
        if std::mem::take(&mut self.open_details) {
            side.open();
        }
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

    /// The uploads' count (while any group is unfinished) and a refused drop, on the status bar.
    fn uploads_status_ui(&mut self, ui: &mut egui::Ui) {
        if let Some((text, since)) = &self.notice {
            if let Some(left) = NOTICE_FOR.checked_sub(since.elapsed()) {
                ui.colored_label(ui.visuals().warn_fg_color, text.as_str());
                ui.ctx().request_repaint_after(left);
            } else {
                self.notice = None;
            }
        }
        let Some(uploads) = self.connection.as_ref().and_then(|c| c.session.uploads()) else {
            return;
        };
        let snap = uploads.snapshot();
        let Some((done, total, awaiting)) =
            crate::uploads::status_counts(&snap.groups, &snap.summary)
        else {
            return;
        };
        let text = if awaiting > 0 {
            tr!(
                ui,
                "asteria-photos-status-uploads-awaiting",
                done = done,
                total = total,
                count = awaiting
            )
        } else {
            tr!(
                ui,
                "asteria-photos-status-uploads",
                done = done,
                total = total
            )
        };
        let button = ui.add(egui::Button::new(text).frame(false));
        if button
            .on_hover_text(tr!(ui, "asteria-photos-status-uploads-hover"))
            .clicked()
        {
            self.open_screen = Some(Screen::Uploads);
        }
    }

    /// The server in use and its state, with a menu of the servers to switch to. `compact`
    /// shows only the state's icon (the bottom bar has little room).
    fn switcher_ui(&mut self, ui: &mut egui::Ui, compact: bool) {
        let session = self.connection.as_ref().map(|c| &c.session);
        let degraded = session.and_then(|s| s.degraded());
        let detail = match (session.map(|s| s.state()), degraded) {
            (Some(SessionState::Failed(e)), _) | (_, Some(e)) => error_detail(e),
            _ => None,
        };
        let (icon, swatch, status) = match session.map(|s| s.state()) {
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
            Some(SessionState::Ready { .. }) => match degraded {
                // Connected, but the latest request found the server gone or the token refused.
                Some(e) => {
                    let reason = error_text(ui.ctx(), e);
                    (
                        icons::cloud_off(),
                        Swatch::Orange,
                        tr!(ui, "asteria-photos-status-degraded", reason = reason),
                    )
                }
                None => (
                    icons::circle_check(),
                    Swatch::Green,
                    tr!(ui, "asteria-photos-status-connected"),
                ),
            },
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
        let mut hover = tr!(
            ui,
            "asteria-photos-status-hover",
            name = name.as_str(),
            status = status.as_str()
        );
        if let Some(detail) = detail {
            hover.push('\n');
            hover.push_str(&detail);
        }
        menu.0.on_hover_text(hover);
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

/// How long the status bar keeps a notice.
const NOTICE_FOR: Duration = Duration::from_secs(8);

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
    fn screen_ui(&mut self, ui: &mut egui::Ui, screen: Screen, side: &mut Side) {
        match screen {
            Screen::Library => self.library_ui(ui, side),
            Screen::Uploads => {
                let c = self.connection.as_mut();
                match c {
                    Some(c) => crate::uploads::ui(ui, c.session.uploads(), &mut c.uploads),
                    None => crate::uploads::ui(ui, None, &mut Default::default()),
                }
            }
            Screen::Settings => self.settings_ui(ui),
        }
    }

    /// The selected item's details, beside the library (the side pane is the shell's, so
    /// other screens show a word about it).
    fn side_ui(&mut self, ui: &mut egui::Ui, screen: Screen, _side: &mut Side) {
        let ready = self
            .connection
            .as_mut()
            .filter(|c| matches!(c.session.state(), SessionState::Ready { .. }));
        match (screen, ready) {
            (Screen::Library, Some(c)) => c.browse.detail_ui(ui, &mut c.session),
            (Screen::Library, None) => {
                ui.weak(tr!(ui, "asteria-photos-detail-none"));
            }
            (Screen::Uploads | Screen::Settings, _) => {
                ui.weak(tr!(ui, "asteria-photos-detail-other-screen"));
            }
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
        self.uploads_status_ui(ui);
    }
}

impl corundum_framework::eframe::App for PhotosApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut corundum_framework::eframe::Frame) {
        self.show(ui);
    }
}
