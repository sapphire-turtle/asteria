//! The spike's screens and the rail entries that open them.

pub mod audio;
pub mod display;
pub mod report;

use corundum_gui::{Destination, egui, icons};

use crate::report::Report;

/// One screen of the spike, a destination on the shell's rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// Safe area, Japanese text, touch and keyboard checks.
    Display,
    /// Background audio, interruptions and lock-screen controls.
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
pub fn not_implemented(ui: &mut egui::Ui, name: &str) {
    ui.heading(name);
    ui.label("未実装");
}

/// What the person answered for one checklist item.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Unchecked,
    Ok,
    Ng,
}

impl Answer {
    fn as_str(self) -> &'static str {
        match self {
            Answer::Unchecked => "unchecked",
            Answer::Ok => "ok",
            Answer::Ng => "ng",
        }
    }
}

/// Record every item of `checks` (`(report key, label)`) as `unchecked` under
/// `check.<group>.<key>`.
pub fn reset_checklist(group: &str, checks: &[(&str, &str)], report: &mut Report) {
    for (key, _) in checks {
        report.set(&format!("check.{group}.{key}"), Answer::Unchecked.as_str());
    }
}

/// An OK/NG row per item of `checks`; a changed answer goes to `check.<group>.<key>`.
pub fn checklist(
    ui: &mut egui::Ui,
    group: &str,
    checks: &[(&str, &str)],
    answers: &mut [Answer],
    report: &mut Report,
) {
    for ((key, name), answer) in checks.iter().zip(answers) {
        let before = *answer;
        ui.horizontal(|ui| {
            ui.label(*name);
            ui.selectable_value(answer, Answer::Ok, "OK");
            ui.selectable_value(answer, Answer::Ng, "NG");
        });
        if *answer != before {
            report.set(&format!("check.{group}.{key}"), answer.as_str());
        }
    }
}
