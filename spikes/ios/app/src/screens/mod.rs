//! The spike's screens and the rail entries that open them.

pub mod display;
pub mod report;

use corundum_gui::{Destination, icons};

/// One screen of the spike, a destination on the shell's rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// Safe area, Japanese text, touch and keyboard checks.
    Display,
    /// Background audio and interruptions (not built yet).
    Audio,
    /// Photo library access (not built yet).
    Photos,
    /// Opus decoding on the device (not built yet).
    Opus,
    /// The report text and its copy button.
    Report,
}

impl Screen {
    /// The rail entries, in order. corundum-gui has no display, speaker, image, audio
    /// or clipboard icon, so these borrow the closest Lucide icons it does have.
    pub fn destinations() -> Vec<Destination<Screen>> {
        vec![
            Destination::new(Screen::Display, icons::info(), "Display"),
            Destination::new(Screen::Audio, icons::chevron_right(), "Audio"),
            Destination::new(Screen::Photos, icons::folder(), "Photos"),
            Destination::new(Screen::Opus, icons::refresh_cw(), "Opus"),
            Destination::new(Screen::Report, icons::file_text(), "Report"),
        ]
    }
}

/// The body of a screen that is not built yet.
pub fn not_implemented(ui: &mut corundum_gui::egui::Ui, name: &str) {
    ui.heading(name);
    ui.label("未実装");
}
