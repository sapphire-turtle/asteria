//! Settings pages the client apps share.

pub mod servers;

use corundum_framework::egui;

/// A Settings page an app is handed from outside (the desktop's "Local server", which the
/// phone builds have no use for). The app lists it after its servers.
pub trait ExtraSettingsPage {
    /// The page's name in the settings list.
    fn label(&self, ctx: &egui::Context) -> String;

    /// Draw the page.
    fn ui(&mut self, ui: &mut egui::Ui);
}
