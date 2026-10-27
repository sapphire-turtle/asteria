//! [`Connection`]: everything tied to the server in use, dropped whole on a switch.

use asteria_framework_client::profiles::ServerProfile;
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::{Session, SessionDirs, SessionState};
use corundum_framework::egui;

use crate::browse::BrowseState;
use crate::uploads::UploadsView;

/// One server in use: its [`Session`] (requests, images, cache) and its [`BrowseState`] (the
/// folder shown, its pages, the selection, the item opened, the polling). Dropping it drops
/// both, and with them every request still running for that server, so nothing the old
/// server sends can reach the next one's screen.
#[derive(Debug)]
pub struct Connection {
    pub(crate) session: Session,
    pub(crate) browse: BrowseState,
    pub(crate) uploads: UploadsView,
    /// Opened by the app itself after the server's library changed. Such a connection
    /// is not reopened again by itself, so a server that keeps changing cannot loop.
    reopened: bool,
}

impl Connection {
    /// Connects to `profile` and asks for its library's root.
    pub(crate) fn open(
        profile: &ServerProfile,
        ctx: &egui::Context,
        dirs: SessionDirs,
        reopened: bool,
    ) -> Connection {
        let mut session = Session::open_with_dirs(
            profile,
            asteria_photos_api::client_info(),
            ctx.clone(),
            dirs,
        );
        let mut browse = BrowseState::new();
        // Queued behind the handshake; it fails with the handshake if that fails.
        browse.open(&mut session, "");
        Connection {
            session,
            browse,
            uploads: UploadsView::default(),
            reopened,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn browse(&self) -> &BrowseState {
        &self.browse
    }

    /// Hands the session's replies to the browse state.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        for reply in self.session.poll() {
            self.browse.handle(&reply);
        }
        self.browse.tick(&mut self.session, ctx);
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
