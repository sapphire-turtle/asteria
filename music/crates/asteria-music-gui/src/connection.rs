//! [`Connection`]: everything tied to the server in use, dropped whole on a switch.

use std::time::{Duration, Instant};

use asteria_framework_client::profiles::ServerProfile;
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::{Request, RequestId, Session, SessionDirs, SessionState};
use corundum_framework::egui;

use crate::library::LibraryState;

/// How often `library.info` is asked again while the server scans.
pub const SCANNING_POLL: Duration = Duration::from_secs(2);
/// How often it is asked otherwise, to notice changes the server's watcher found.
pub const IDLE_POLL: Duration = Duration::from_secs(30);

/// One server in use: its [`Session`] (requests, images, cache) and its [`LibraryState`]
/// (the lists, what is open, the polling). Dropping it drops both, and with them every
/// request still running for that server, so nothing the old server sends can reach the
/// next one's screen.
#[derive(Debug)]
pub struct Connection {
    pub(crate) session: Session,
    pub(crate) library: LibraryState,
    /// The `library.info` waited for.
    info: Option<RequestId>,
    next_info: Instant,
    /// Opened by the app itself after the server's library changed. Such a connection
    /// is not reopened again by itself, so a server that keeps changing cannot loop.
    reopened: bool,
}

impl Connection {
    /// Connects to `profile` and asks for the first page of each list.
    pub(crate) fn open(
        profile: &ServerProfile,
        ctx: &egui::Context,
        dirs: SessionDirs,
        reopened: bool,
    ) -> Connection {
        let mut session =
            Session::open_with_dirs(profile, asteria_music_api::client_info(), ctx.clone(), dirs);
        let mut library = LibraryState::new();
        // Queued behind the handshake; they fail with the handshake if that fails.
        library.start(&mut session);
        Connection {
            session,
            library,
            info: None,
            next_info: Instant::now() + IDLE_POLL,
            reopened,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn library(&self) -> &LibraryState {
        &self.library
    }

    /// Hands the session's replies to the library, and keeps its upkeep.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        for reply in self.session.poll() {
            if self.info == Some(reply.id) {
                self.info = None;
                continue;
            }
            self.library.handle(&reply);
        }
        self.tick(ctx);
    }

    /// Ask `library.info` now (unless it is already asked). The session compares the answer
    /// with what it knew: a new epoch, a scan that ended or a new item count between scans
    /// makes the lists be fetched again, another library fails the session.
    pub(crate) fn refresh_library(&mut self) {
        if self.info.is_none() {
            self.info = Some(self.session.request(Request::LibraryInfo));
        }
    }

    /// Per-frame upkeep: fetch the lists again (keeping them on screen until the new first
    /// pages arrive) when the session says the library changed (see
    /// [`Session::take_refetch_listing`]), and ask `library.info` again on schedule (often
    /// while the server scans). Asks egui to wake for the next check.
    fn tick(&mut self, ctx: &egui::Context) {
        if self.session.take_refetch_listing() {
            self.library.reload(&mut self.session);
        }
        let SessionState::Ready { library } = self.session.state() else {
            return;
        };
        let interval = if library.scanning {
            SCANNING_POLL
        } else {
            IDLE_POLL
        };
        let now = Instant::now();
        // A shorter interval (a scan began) takes effect at once.
        self.next_info = self.next_info.min(now + interval);
        if self.info.is_none() && now >= self.next_info {
            self.refresh_library();
            self.next_info = now + interval;
        }
        ctx.request_repaint_after(self.next_info.saturating_duration_since(now));
    }

    /// Whether the server now serves another library (and this connection may be reopened).
    pub(crate) fn library_changed(&self) -> bool {
        !self.reopened
            && matches!(
                self.session.state(),
                SessionState::Failed(ClientError::LibraryChanged)
            )
    }
}
