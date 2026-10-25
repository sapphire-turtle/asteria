//! The audio screen (spec section 8, item 2): play a tone or the Opus sample, pause and
//! stop it, see the engine's state, and answer the background-playback checklist.

use corundum_framework::egui;

use super::{Answer, checklist, reset_checklist};
use crate::audio::{Command, Engine, PlayState, Source};
use crate::report::Report;

/// The things the person holding the device confirms, as `(report key, label)`.
const CHECKS: &[(&str, &str)] = &[
    ("locked_2min", "ロックしたまま 2 分鳴り続ける"),
    ("lock_screen_controls", "ロック画面の再生・一時停止"),
    ("control_center", "コントロールセンターの再生・一時停止"),
    (
        "interruption_resume",
        "着信や Siri で止まり、終わると再開する",
    ),
    ("unplug_pauses", "イヤホンを外すと止まる"),
    ("background_foreground", "ホームに戻って戻っても鳴り続ける"),
];

/// State of the audio screen.
pub struct AudioScreen {
    answers: Vec<Answer>,
}

impl AudioScreen {
    /// A fresh screen; every check starts as `unchecked` in `report`.
    pub fn new(report: &mut Report) -> Self {
        reset_checklist("audio", CHECKS, report);
        Self {
            answers: vec![Answer::Unchecked; CHECKS.len()],
        }
    }

    /// Draw the screen, sending button presses to `engine` and check answers to `report`.
    pub fn ui(&mut self, ui: &mut egui::Ui, engine: &Engine, report: &mut Report) {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.heading("Audio");
                let state = engine.state();
                ui.monospace(format!("状態: {}", state_text(state)));
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Tone を再生").clicked() {
                        engine.send(Command::Play(Source::Tone));
                    }
                    if ui.button("Opus を再生").clicked() {
                        engine.send(Command::Play(Source::OpusSample));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    let toggle = match state {
                        PlayState::Paused { .. } => "再開",
                        _ => "一時停止",
                    };
                    if ui
                        .add_enabled(state != PlayState::Stopped, egui::Button::new(toggle))
                        .clicked()
                    {
                        engine.send(Command::Toggle);
                    }
                    if ui
                        .add_enabled(state != PlayState::Stopped, egui::Button::new("停止"))
                        .clicked()
                    {
                        engine.send(Command::Stop);
                    }
                });
                ui.separator();
                checklist(ui, "audio", CHECKS, &mut self.answers, report);
            });
    }
}

fn state_text(state: PlayState) -> String {
    match state {
        PlayState::Stopped => "停止中".to_owned(),
        PlayState::Playing(source) => format!("再生中 ({})", source.title()),
        PlayState::Paused {
            source,
            by_interruption,
        } => {
            let why = if by_interruption {
                "中断"
            } else {
                "一時停止"
            };
            format!("{why} ({})", source.title())
        }
    }
}
