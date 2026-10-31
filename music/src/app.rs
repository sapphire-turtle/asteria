//! [`MusicApp`]: the registered servers, the connection to the one in use, the player, and the
//! screens.
//!
//! Everything tied to a server lives in a [`Connection`]: its [`Session`] (requests, images,
//! cache) and its [`LibraryState`](crate::library::LibraryState) (the lists, what is open, the
//! polling). Switching servers drops the connection whole and opens a new one, so nothing the
//! old server sends can reach the new screen. The [`Player`] does not belong to a connection;
//! what the screens ask to play reaches it as [`Command`]s.
//!
//! The servers are kept in `servers.toml`. It is read once, before the first frame; every
//! change is written by a background thread, so the UI thread does no disk I/O.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use asteria_framework_client::errors::{error_detail, error_text};
use asteria_framework_client::profiles::{ProfileId, Profiles, ServerProfile, Token};
use asteria_framework_client::saver::Saver;
use asteria_framework_client::session::{SessionDirs, SessionState};
use asteria_framework_client::settings::servers::{ServersChange, ServersPage};
use asteria_framework_client::widgets::server_switcher::{ServerSwitcher, SwitcherAction};
use asteria_music_api::{AlbumId, ArtistId, PlaylistId};
use corundum_framework::egui;
use corundum_framework::gui::settings::{ListItem, SettingsLayout, SettingsSection};
use corundum_framework::gui::{COMPACT_WIDTH, Destination, Shell, ShellApp, Side, icons, tr};

pub use crate::connection::Connection;
use crate::now_playing::{self, BarAction};
use crate::player::{Command, Endpoint, Player, Quality, feed};
use crate::queue::{self, QueueAction};
use crate::settings::{PlaybackPage, PlaybackSettings};

/// The screens: the browse screens and the queue on the rail, Settings at its foot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Albums,
    Artists,
    Playlists,
    Queue,
    Settings,
}

/// The pages of the Settings screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Servers,
    Playback,
}

/// Where the app keeps its files.
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// The registered servers and the one in use.
    pub servers: PathBuf,
    /// The playback settings (the stream quality and the like).
    pub playback: PathBuf,
    /// The root of the cover cache (one directory per library under it); `None` keeps no
    /// cache on disk.
    pub cache: Option<PathBuf>,
}

/// The app. Hold one; call [`show`](Self::show) every frame with the window's root `Ui`.
pub struct MusicApp {
    shell: Shell<Screen>,
    state: State,
}

/// Everything but the shell, which draws it.
struct State {
    ctx: egui::Context,
    paths: AppPaths,
    player: Player,
    /// After `player`: the directories are removed once the player has stopped using them.
    stream_dirs: StreamDirs,
    profiles: Profiles,
    connection: Option<Connection>,
    servers_page: ServersPage,
    playback_page: PlaybackPage,
    playback_saver: Option<Saver<PlaybackSettings>>,
    /// Whether the player was handed the connection's endpoint.
    endpoint_sent: bool,
    /// Where the seek bar is dragged to (seconds), until the drag ends.
    seeking: Option<f64>,
    settings: SettingsLayout<Page>,
    /// `None` when `servers.toml` could not be read (saving would overwrite it) or the save
    /// thread could not start; `save_blocked` says why.
    saver: Option<Saver<Profiles>>,
    save_blocked: Option<String>,
    /// A screen to open after this frame (from the status bar's menu, say).
    open_screen: Option<Screen>,
    /// Whether the shell lays out for a phone this frame (no status bar).
    compact: bool,
}

impl MusicApp {
    /// Reads `servers.toml` (this is the one disk read on the UI thread, before the first
    /// frame) and connects to the server used last. With no server registered, it opens on
    /// Settings, ready to add one.
    pub fn new(paths: AppPaths, ctx: &egui::Context, player: Player) -> MusicApp {
        crate::i18n::register(ctx);
        // Ruling R11: read on the UI thread, but before the first frame; the file is tiny.
        let (profiles, load_error) = match Profiles::load(&paths.servers) {
            Ok(p) => (p, None),
            Err(e) => {
                tracing::warn!(path = %paths.servers.display(), error = %e, "reading servers.toml failed");
                (Profiles::default(), Some(e.to_string()))
            }
        };
        let mut servers_page = ServersPage::new(&profiles, asteria_music_api::client_info());
        let (saver, save_blocked) = match load_error {
            Some(e) => (
                None,
                Some(tr!(ctx, "asteria-music-load-failed", reason = e)),
            ),
            None => match spawn_saver(paths.servers.clone(), ctx.clone()) {
                Ok(s) => (Some(s), None),
                Err(e) => (
                    None,
                    Some(tr!(
                        ctx,
                        "asteria-music-save-failed",
                        reason = e.to_string()
                    )),
                ),
            },
        };
        if let Some(why) = &save_blocked {
            servers_page.show_error(why.clone());
        }
        let playback = PlaybackSettings::load(&paths.playback);
        player.send(Command::SetQuality(quality_of(&playback)));
        player.send(Command::SetVolume(playback.volume));
        let playback_saver = match spawn_playback_saver(paths.playback.clone(), ctx.clone()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "starting the playback settings saver failed");
                None
            }
        };
        let shell = Shell::new(vec![
            Destination::new(
                Screen::Albums,
                icons::music(),
                "asteria-music-screen-albums",
            ),
            Destination::new(
                Screen::Artists,
                egui::include_image!("../assets/icons/mic-vocal.svg"),
                "asteria-music-screen-artists",
            ),
            Destination::new(
                Screen::Playlists,
                icons::file_text(),
                "asteria-music-screen-playlists",
            ),
            Destination::new(
                Screen::Queue,
                icons::list_music(),
                "asteria-music-screen-queue",
            ),
        ])
        .with_footer(vec![Destination::new(
            Screen::Settings,
            icons::settings(),
            "asteria-music-screen-settings",
        )]);
        let mut state = State {
            ctx: ctx.clone(),
            paths,
            player,
            stream_dirs: StreamDirs::default(),
            profiles,
            connection: None,
            servers_page,
            playback_page: PlaybackPage::new(playback),
            playback_saver,
            endpoint_sent: false,
            seeking: None,
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
        let mut app = MusicApp { shell, state };
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

    /// The player the screens send what to play to.
    pub fn player(&self) -> &Player {
        &self.state.player
    }

    /// The screen shown.
    pub fn current_screen(&self) -> Option<Screen> {
        self.shell.current()
    }

    /// Show `screen`.
    pub fn open_screen(&mut self, screen: Screen) {
        self.shell.open(screen);
        self.state.ctx.request_repaint();
    }

    /// Use the server `id` from now on: saved as the one in use, connected afresh. Choosing the
    /// server in use again only reconnects if it had failed or stopped answering
    /// ([`Session::degraded`](asteria_framework_client::session::Session::degraded)).
    pub fn switch_to(&mut self, id: ProfileId) {
        self.state.switch_to(id);
    }

    /// Ask the server for `library.info` now, rather than at the next scheduled check.
    pub fn refresh_library(&mut self) {
        if let Some(c) = &mut self.state.connection {
            c.refresh_library();
        }
    }

    /// Open the album `id` on the Albums screen.
    pub fn open_album(&mut self, id: AlbumId) {
        if let Some(c) = &mut self.state.connection {
            c.library.open_album(&mut c.session, id);
        }
    }

    /// Close the album opened on the Albums screen.
    pub fn close_album(&mut self) {
        if let Some(c) = &mut self.state.connection {
            c.library.close_album();
        }
    }

    /// Open the artist `id` on the Artists screen.
    pub fn open_artist(&mut self, id: ArtistId) {
        if let Some(c) = &mut self.state.connection {
            c.library.open_artist(&mut c.session, id);
        }
    }

    /// Open the album `id` from the artist shown.
    pub fn open_artist_album(&mut self, id: AlbumId) {
        if let Some(c) = &mut self.state.connection {
            c.library.open_artist_album(&mut c.session, id);
        }
    }

    /// Open the playlist `id` on the Playlists screen.
    pub fn open_playlist(&mut self, id: PlaylistId) {
        if let Some(c) = &mut self.state.connection {
            c.library.open_playlist(&mut c.session, id);
        }
    }

    /// Ask for the next page of albums, as the grid does near its end.
    pub fn load_more_albums(&mut self) {
        if let Some(c) = &mut self.state.connection {
            c.library.load_more_albums(&mut c.session);
        }
    }

    /// Try the albums again after a page failed, as the "try again" button does.
    pub fn retry_albums(&mut self) {
        if let Some(c) = &mut self.state.connection {
            c.library.retry_albums(&mut c.session);
        }
    }

    /// The playback settings as they stand.
    pub fn playback_settings(&self) -> &PlaybackSettings {
        &self.state.playback_page.settings
    }

    /// Play or pause (the bar's button).
    pub fn toggle(&mut self) {
        self.state.apply_bar(BarAction::Toggle);
    }

    /// The next track.
    pub fn next(&mut self) {
        self.state.apply_bar(BarAction::Next);
    }

    /// The previous track, or the start of this one.
    pub fn previous(&mut self) {
        self.state.apply_bar(BarAction::Previous);
    }

    /// To `ms` into the track.
    pub fn seek(&mut self, ms: u64) {
        self.state.apply_bar(BarAction::Seek(ms));
    }

    /// Set the volume (0.0..=1.0); kept in `playback.toml`.
    pub fn set_volume(&mut self, volume: f32) {
        self.state.apply_bar(BarAction::SetVolume(volume));
    }

    /// Stream the low-quality rendition from the next track on; kept in `playback.toml`.
    pub fn set_low_quality(&mut self, low: bool) {
        self.state.playback_page.settings.low_quality = low;
        self.state.apply_playback_change();
    }

    /// Play the queue's entry `index`.
    pub fn jump(&mut self, index: usize) {
        self.state.apply_queue(QueueAction::Jump(index));
    }

    /// Move the queue's entry `from` to `to`.
    pub fn move_queue_entry(&mut self, from: usize, to: usize) {
        self.state.apply_queue(QueueAction::Move { from, to });
    }

    /// Remove the queue's entry `index`.
    pub fn remove_queue_entry(&mut self, index: usize) {
        self.state.apply_queue(QueueAction::Remove(index));
    }

    /// The Settings page of the servers, as the user works it.
    pub fn servers_page_mut(&mut self) -> &mut ServersPage {
        &mut self.state.servers_page
    }
}

impl State {
    /// Opens a connection to the active server, dropping the one there was.
    fn connect(&mut self, reopened: bool) {
        // The old session goes first: its pending results die with it. So does what the
        // player holds of its server: the queue, the streams.
        self.connection = None;
        if self.endpoint_sent {
            self.endpoint_sent = false;
            self.player.send(Command::SetEndpoint(None));
        }
        self.connection = self.profiles.active().map(|profile| {
            let dirs = SessionDirs {
                cache: self.paths.cache.clone(),
                data: None,
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
            Some(saver) => saver.save(self.profiles.clone()),
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
            while let Some(e) = saver.take_error() {
                let why = tr!(self.ctx, "asteria-music-save-failed", reason = e);
                self.servers_page.show_error(why);
            }
        }
        if let Some(saver) = &self.playback_saver {
            while let Some(e) = saver.take_error() {
                let why = tr!(self.ctx, "asteria-music-playback-save-failed", reason = e);
                self.playback_page.show_error(why);
            }
        }
        if let Some(c) = &mut self.connection {
            c.poll(&self.ctx);
        }
        self.send_endpoint();
        // The server now serves another library: start again with it.
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
                "asteria-music-save-failed",
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

    /// A browse screen: the connection's state, or the screen once it is ready.
    fn library_ui(&mut self, ui: &mut egui::Ui, screen: Screen) {
        let Some(c) = &mut self.connection else {
            egui::CentralPanel::default().show(ui, |ui| {
                let hint = if self.profiles.servers.is_empty() {
                    tr!(ui, "asteria-music-library-no-servers")
                } else {
                    tr!(ui, "asteria-music-library-no-active")
                };
                ui.label(hint);
                if ui
                    .button(tr!(ui, "asteria-music-library-open-servers"))
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
                            "asteria-music-library-connect-failed",
                            name = name.as_str(),
                            reason = reason
                        ),
                    );
                    if let Some(detail) = detail {
                        label.on_hover_text(detail);
                    }
                    ui.horizontal(|ui| {
                        retry = ui.button(tr!(ui, "asteria-music-library-retry")).clicked();
                        manage = ui
                            .button(tr!(ui, "asteria-music-library-open-servers"))
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
                            "asteria-music-library-connecting",
                            name = name.as_str()
                        ));
                    });
                });
            }
            SessionState::Ready { .. } => {
                let mut command = None;
                egui::CentralPanel::default().show(ui, |ui| {
                    command = match screen {
                        Screen::Albums => c.library.albums_ui(ui, &mut c.session),
                        Screen::Artists => c.library.artists_ui(ui, &mut c.session),
                        Screen::Playlists => c.library.playlists_ui(ui, &mut c.session),
                        Screen::Queue | Screen::Settings => None,
                    };
                });
                if let Some(command) = command {
                    self.send(command);
                }
            }
        }
    }

    fn send(&self, command: Command) {
        self.player.send(command);
    }

    /// Once the session is ready, tells the player which server to stream from.
    fn send_endpoint(&mut self) {
        if self.endpoint_sent {
            return;
        }
        let Some(c) = &self.connection else {
            return;
        };
        let SessionState::Ready { library } = c.session.state() else {
            return;
        };
        let Some(profile) = self.profiles.active() else {
            return;
        };
        let library_id = library.library_id.clone();
        let (url, token) = (profile.url.clone(), profile.token.clone());
        let Some(stream_dir) = self
            .stream_dirs
            .get(self.paths.cache.as_deref(), &library_id)
        else {
            return;
        };
        self.endpoint_sent = true;
        self.player.send(Command::SetEndpoint(Some(Endpoint {
            url,
            token,
            library_id,
            stream_dir,
        })));
    }

    fn apply_bar(&mut self, action: BarAction) {
        match action {
            BarAction::Toggle => self.send(Command::Toggle),
            BarAction::Next => self.send(Command::Next),
            BarAction::Previous => self.send(Command::Previous),
            BarAction::Seek(ms) => self.send(Command::Seek(ms)),
            BarAction::SetVolume(v) => {
                let v = if v.is_nan() { 1.0 } else { v.clamp(0.0, 1.0) };
                self.send(Command::SetVolume(v));
                self.playback_page.settings.volume = v;
                self.apply_playback_change();
            }
        }
    }

    fn apply_queue(&mut self, action: QueueAction) {
        self.send(match action {
            QueueAction::Jump(i) => Command::Jump(i),
            QueueAction::Move { from, to } => Command::Move { from, to },
            QueueAction::Remove(i) => Command::Remove(i),
        });
    }

    /// Sends the quality and volume the page holds to the player, and writes them.
    fn apply_playback_change(&mut self) {
        let settings = self.playback_page.settings.clone();
        self.send(Command::SetQuality(quality_of(&settings)));
        match &self.playback_saver {
            Some(saver) => saver.save(settings),
            None => self.playback_page.show_error(tr!(
                self.ctx,
                "asteria-music-playback-save-failed",
                reason = "no save thread"
            )),
        }
    }

    /// The playback bar, a panel at the foot of the screen.
    fn bar_ui(&mut self, ui: &mut egui::Ui) {
        let state = self.player.state();
        let images = self.connection.as_mut().map(|c| c.session.images());
        let action = now_playing::show(ui, &state, images, self.compact, &mut self.seeking);
        if let Some(action) = action {
            self.apply_bar(action);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let items = [
            ListItem::Entry {
                page: Page::Servers,
                label: self.servers_page.label(ui.ctx()),
                dirty: self.servers_page.dirty(),
            },
            ListItem::Entry {
                page: Page::Playback,
                label: self.playback_page.label(ui.ctx()),
                dirty: false,
            },
        ];
        let page = &mut self.servers_page;
        let playback = &mut self.playback_page;
        self.settings.ui(ui, &items, |ui, open| match open {
            Page::Servers => page.ui(ui),
            Page::Playback => playback.ui(ui),
        });
        if self.playback_page.take_change().is_some() {
            self.apply_playback_change();
        }
    }

    /// The server in use and its state, with a menu of the servers to switch to. `compact`
    /// shows only the state's icon (the bottom bar has little room).
    fn switcher_ui(&mut self, ui: &mut egui::Ui, compact: bool) {
        let action = ServerSwitcher {
            profiles: &self.profiles,
            session: self.connection.as_ref().map(|c| &c.session),
            compact,
        }
        .show(ui);
        match action {
            Some(SwitcherAction::Switch(id)) => self.switch_to(id),
            Some(SwitcherAction::Manage) => self.open_servers_settings(),
            None => {}
        }
    }
}

fn quality_of(settings: &PlaybackSettings) -> Quality {
    if settings.low_quality {
        Quality::Low
    } else {
        Quality::Original
    }
}

/// A saver that writes `playback.toml` on the thread `music-playback-save`.
fn spawn_playback_saver(
    path: PathBuf,
    ctx: egui::Context,
) -> std::io::Result<Saver<PlaybackSettings>> {
    Saver::spawn(
        "music-playback-save",
        ctx,
        move |settings: &PlaybackSettings| {
            settings.save(&path).map_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "saving playback.toml failed");
                e.to_string()
            })
        },
    )
}

/// The directories this process streams tracks into, one per library, removed when dropped.
#[derive(Default)]
struct StreamDirs {
    dirs: HashMap<String, PathBuf>,
    /// Library ids whose directory could not be made: warned about once, not retried.
    failed: HashSet<String>,
}

impl StreamDirs {
    /// The directory for `library_id`: under the cache `root`, or if there is none or it
    /// cannot be made, under the system's temporary directory.
    fn get(&mut self, root: Option<&std::path::Path>, library_id: &str) -> Option<PathBuf> {
        if let Some(dir) = self.dirs.get(library_id) {
            return Some(dir.clone());
        }
        if self.failed.contains(library_id) {
            return None;
        }
        let fallback = std::env::temp_dir().join("asteria-music");
        let made = root
            .and_then(|r| {
                feed::stream_dir(r, library_id)
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, "making the stream directory in the cache failed");
                    })
                    .ok()
            })
            .or_else(|| {
                feed::stream_dir(&fallback, library_id)
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, "making the stream directory failed");
                    })
                    .ok()
            });
        let Some(made) = made else {
            self.failed.insert(library_id.to_string());
            return None;
        };
        self.dirs.insert(library_id.to_string(), made.clone());
        Some(made)
    }
}

impl Drop for StreamDirs {
    fn drop(&mut self) {
        for dir in self.dirs.values() {
            // `remove_dir` would fail on the `.alive` marker inside.
            if let Err(e) = std::fs::remove_dir_all(dir) {
                tracing::warn!(path = %dir.display(), error = %e, "removing the stream directory failed");
            }
        }
    }
}

/// A saver that writes `servers.toml` on the thread `music-servers-save`.
fn spawn_saver(path: PathBuf, ctx: egui::Context) -> std::io::Result<Saver<Profiles>> {
    Saver::spawn("music-servers-save", ctx, move |profiles: &Profiles| {
        // The error never holds the file's contents, so no token.
        profiles.save(&path).map_err(|e| {
            tracing::warn!(path = %path.display(), error = %e, "saving servers.toml failed");
            e.to_string()
        })
    })
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
        self.bar_ui(ui);
        match screen {
            Screen::Albums | Screen::Artists | Screen::Playlists => self.library_ui(ui, screen),
            Screen::Queue => {
                let state = self.player.state();
                let mut action = None;
                egui::CentralPanel::default().show(ui, |ui| {
                    action = queue::ui(ui, &state);
                });
                if let Some(action) = action {
                    self.apply_queue(action);
                }
            }
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

impl corundum_framework::eframe::App for MusicApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut corundum_framework::eframe::Frame) {
        self.show(ui);
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn an_unsafe_library_id_gets_no_stream_dir_and_is_not_retried() {
        let root = TempDir::new().unwrap();
        let mut dirs = StreamDirs::default();
        assert!(dirs.get(Some(root.path()), "../x").is_none());
        assert!(dirs.failed.contains("../x"));
        assert!(dirs.get(Some(root.path()), "../x").is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        // Another library is still tried.
        let dir = dirs.get(Some(root.path()), "lib").unwrap();
        assert!(dir.starts_with(root.path()));
    }
}
