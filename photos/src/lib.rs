//! The photos client: the app, the browse view and the detail view. `main.rs` only opens the window.

use corundum_framework::{
    egui::{self, Ui},
    gui::{Destination, Shell, ShellApp, Side, icons},
};

/// The screens on the rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Photos,
}

/// The app: for now an empty shell.
pub struct PhotosApp {
    shell: Shell<Screen>,
}

impl PhotosApp {
    pub fn new() -> PhotosApp {
        PhotosApp {
            shell: Shell::new(vec![Destination::new(
                Screen::Photos,
                icons::folder(),
                "photos",
            )]),
        }
    }
}

impl Default for PhotosApp {
    fn default() -> Self {
        PhotosApp::new()
    }
}

struct Screens;

impl ShellApp<Screen> for Screens {
    fn screen_ui(&mut self, ui: &mut Ui, _screen: Screen, _side: &mut Side) {
        egui::CentralPanel::default().show(ui, |_| {});
    }
}

impl corundum_framework::eframe::App for PhotosApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut corundum_framework::eframe::Frame) {
        self.shell.show(ui, &mut Screens);
    }
}
