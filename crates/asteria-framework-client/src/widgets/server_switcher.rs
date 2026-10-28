//! [`ServerSwitcher`]: the server in use and its state, with a menu to switch servers.

use corundum_framework::egui;
use corundum_framework::gui::theme::{Swatch, Tone};
use corundum_framework::gui::{icons, tr};

use crate::errors::{error_detail, error_text};
use crate::profiles::{ProfileId, Profiles};
use crate::session::{Session, SessionState};

/// The server in use and its state, with a menu of the servers to switch to. `compact`
/// shows only the state's icon (the bottom bar has little room).
pub struct ServerSwitcher<'a> {
    pub profiles: &'a Profiles,
    /// The connection to the server in use, if there is one.
    pub session: Option<&'a Session>,
    pub compact: bool,
}

/// What the user chose in the switcher's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitcherAction {
    Switch(ProfileId),
    /// Open the servers settings.
    Manage,
}

impl ServerSwitcher<'_> {
    pub fn show(self, ui: &mut egui::Ui) -> Option<SwitcherAction> {
        let ServerSwitcher {
            profiles,
            session,
            compact,
        } = self;
        let degraded = session.and_then(|s| s.degraded());
        let detail = match (session.map(|s| s.state()), degraded) {
            (Some(SessionState::Failed(e)), _) | (_, Some(e)) => error_detail(e),
            _ => None,
        };
        let (icon, swatch, status) = match session.map(|s| s.state()) {
            None => (
                icons::cloud_off(),
                Swatch::Neutral,
                tr!(ui, "asteria-client-status-none"),
            ),
            Some(SessionState::Connecting) => (
                icons::refresh_cw(),
                Swatch::Yellow,
                tr!(ui, "asteria-client-status-connecting"),
            ),
            Some(SessionState::Ready { .. }) => match degraded {
                // Connected, but the latest request found the server gone or the token refused.
                Some(e) => {
                    let reason = error_text(ui.ctx(), e);
                    (
                        icons::cloud_off(),
                        Swatch::Orange,
                        tr!(ui, "asteria-client-status-degraded", reason = reason),
                    )
                }
                None => (
                    icons::circle_check(),
                    Swatch::Green,
                    tr!(ui, "asteria-client-status-connected"),
                ),
            },
            Some(SessionState::Failed(e)) => {
                let reason = error_text(ui.ctx(), e);
                (
                    icons::circle_x(),
                    Swatch::Red,
                    tr!(ui, "asteria-client-status-failed", reason = reason),
                )
            }
        };
        let dot = icons::image_as(ui, icon, 14.0, Tone::Text, swatch);
        let name = match profiles.active() {
            Some(p) => p.name.clone(),
            None => tr!(ui, "asteria-client-status-none"),
        };
        let button = if compact {
            egui::Button::image(dot)
        } else {
            egui::Button::image_and_text(dot, name.as_str())
        };
        let active = profiles.active().map(|p| p.id);
        let servers: Vec<(ProfileId, String)> = profiles
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
                let manage = ui.button(tr!(ui, "asteria-client-status-manage")).clicked();
                if manage {
                    ui.close();
                }
                (picked, manage)
            });
        let mut hover = tr!(
            ui,
            "asteria-client-status-hover",
            name = name.as_str(),
            status = status.as_str()
        );
        if let Some(detail) = detail {
            hover.push('\n');
            hover.push_str(&detail);
        }
        menu.0.on_hover_text(hover);
        let inner = menu.1?;
        let (picked, manage) = inner.inner;
        match picked {
            Some(id) => Some(SwitcherAction::Switch(id)),
            None if manage => Some(SwitcherAction::Manage),
            None => None,
        }
    }
}
