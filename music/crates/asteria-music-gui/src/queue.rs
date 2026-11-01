//! The queue screen: the player's queue, the track playing marked. A click on a row plays it,
//! the arrows move it, the cross removes it. It only reports what the user asked for
//! ([`QueueAction`]); the app turns that into a [`Command`](crate::player::Command).

use corundum_framework::egui::{self, Align, Layout, RichText};
use corundum_framework::gui::{icons, tr};

use crate::player::PlayerState;
use crate::tracks::format_duration;

/// What the user asked of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueAction {
    Jump(usize),
    Move { from: usize, to: usize },
    Remove(usize),
}

pub(crate) fn ui(ui: &mut egui::Ui, state: &PlayerState) -> Option<QueueAction> {
    if state.queue.is_empty() {
        ui.weak(tr!(ui, "asteria-music-queue-empty"));
        return None;
    }
    let mut action = None;
    let height = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().button_padding.y * 2.0;
    let last = state.queue.len() - 1;
    egui::ScrollArea::vertical().auto_shrink(false).show_rows(
        ui,
        height,
        state.queue.len(),
        |ui, range| {
            for i in range {
                let entry = &state.queue[i];
                let playing = state.current == Some(i);
                ui.horizontal(|ui| {
                    ui.set_height(height);
                    // Right to left: the buttons and the length first, the rest is the title.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(icons::button(icons::x(), 14.0))
                            .on_hover_text(tr!(ui, "asteria-music-queue-remove"))
                            .clicked()
                        {
                            action = Some(QueueAction::Remove(i));
                        }
                        if ui
                            .add_enabled(i < last, egui::Button::new("\u{2193}"))
                            .on_hover_text(tr!(ui, "asteria-music-queue-down"))
                            .clicked()
                        {
                            action = Some(QueueAction::Move { from: i, to: i + 1 });
                        }
                        if ui
                            .add_enabled(i > 0, egui::Button::new("\u{2191}"))
                            .on_hover_text(tr!(ui, "asteria-music-queue-up"))
                            .clicked()
                        {
                            action = Some(QueueAction::Move { from: i, to: i - 1 });
                        }
                        let length = entry.duration_ms.map(format_duration).unwrap_or_default();
                        ui.weak(length);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let mut title = RichText::new(&entry.title);
                            let mut artist = RichText::new(&entry.artist).weak();
                            if playing {
                                title = title.strong().color(ui.visuals().selection.stroke.color);
                                artist = artist.strong();
                            }
                            let row = ui.add(
                                egui::Label::new(title)
                                    .truncate()
                                    .sense(egui::Sense::click()),
                            );
                            ui.add(egui::Label::new(artist).truncate());
                            if row.clicked() {
                                action = Some(QueueAction::Jump(i));
                            }
                        });
                    });
                });
            }
        },
    );
    action
}
