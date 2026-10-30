//! The playback bar at the foot of every screen: the cover, the title and artist, the
//! transport buttons, the seek bar, the volume, and what is wrong, if anything.
//!
//! Wide, it has everything; narrow (a phone) it is one row of cover, title and play/pause.
//! It draws nothing for an empty queue. It only reports what the user asked for
//! ([`BarAction`]); the app turns that into a [`Command`](crate::player::Command).

use asteria_framework_client::images::{ImageLoader, Preset};
use corundum_framework::egui::{self, Align, Layout, Sense, vec2};
use corundum_framework::gui::{icons, tr};

use crate::cover_grid::paint_picture;
use crate::player::{PlayerError, PlayerState, Status};
use crate::tracks::format_duration;

const COVER: f32 = 48.0;
const COVER_COMPACT: f32 = 36.0;
const ICON: f32 = 20.0;

/// What the user asked of the bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BarAction {
    Toggle,
    Next,
    Previous,
    /// To this many ms into the track.
    Seek(u64),
    SetVolume(f32),
}

/// The text for an error of the player.
pub(crate) fn error_text(ctx: &egui::Context, e: &PlayerError) -> String {
    match e {
        PlayerError::Unplayable { title, reason } => tr!(
            ctx,
            "asteria-music-bar-error-unplayable",
            title = title.as_str(),
            reason = reason.as_str()
        ),
        PlayerError::Network(reason) => {
            tr!(
                ctx,
                "asteria-music-bar-error-network",
                reason = reason.as_str()
            )
        }
        PlayerError::Unauthorized => tr!(ctx, "asteria-music-bar-error-unauthorized"),
        PlayerError::Output(reason) => {
            tr!(
                ctx,
                "asteria-music-bar-error-output",
                reason = reason.as_str()
            )
        }
        PlayerError::TooManyFailures => tr!(ctx, "asteria-music-bar-error-too-many"),
    }
}

/// Draws the bar as a bottom panel of `ui`, unless the queue is empty. `seeking` holds the
/// position (in seconds) the seek bar is dragged to, until the drag ends.
pub(crate) fn show(
    ui: &mut egui::Ui,
    state: &PlayerState,
    images: Option<&mut ImageLoader>,
    compact: bool,
    seeking: &mut Option<f64>,
) -> Option<BarAction> {
    if state.queue.is_empty() {
        return None;
    }
    let mut action = None;
    egui::Panel::bottom("music-now-playing").show(ui, |ui| {
        action = if compact {
            compact_ui(ui, state, images)
        } else {
            wide_ui(ui, state, images, seeking)
        };
    });
    action
}

fn current(state: &PlayerState) -> Option<&crate::player::QueueEntry> {
    state.current.and_then(|i| state.queue.get(i))
}

fn cover(ui: &mut egui::Ui, state: &PlayerState, images: Option<&mut ImageLoader>, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter_at(rect)
        .rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    let picture = match (current(state), images) {
        (Some(entry), Some(images)) => Some(images.request(&entry.media, Preset::Thumb)),
        _ => None,
    };
    paint_picture(ui, rect, picture.as_ref());
}

fn play_pause(ui: &mut egui::Ui, state: &PlayerState) -> bool {
    let playing = matches!(
        state.status,
        Status::Playing | Status::Buffering | Status::Preparing
    );
    let (icon, hint) = if playing {
        (icons::pause(), tr!(ui, "asteria-music-bar-pause"))
    } else {
        (icons::play(), tr!(ui, "asteria-music-bar-play"))
    };
    ui.add(icons::button(icon, ICON))
        .on_hover_text(hint)
        .clicked()
}

fn compact_ui(
    ui: &mut egui::Ui,
    state: &PlayerState,
    images: Option<&mut ImageLoader>,
) -> Option<BarAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        cover(ui, state, images, COVER_COMPACT);
        let title = current(state).map(|e| e.title.as_str()).unwrap_or_default();
        ui.add(egui::Label::new(title).truncate());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if play_pause(ui, state) {
                action = Some(BarAction::Toggle);
            }
        });
    });
    action
}

fn wide_ui(
    ui: &mut egui::Ui,
    state: &PlayerState,
    images: Option<&mut ImageLoader>,
    seeking: &mut Option<f64>,
) -> Option<BarAction> {
    let mut action = None;
    if let Some(e) = &state.error {
        ui.colored_label(ui.visuals().error_fg_color, error_text(ui.ctx(), e));
    }
    ui.horizontal(|ui| {
        cover(ui, state, images, COVER);
        let entry = current(state);
        ui.vertical(|ui| {
            ui.set_width(180.0);
            let title = entry.map(|e| e.title.as_str()).unwrap_or_default();
            ui.add(egui::Label::new(egui::RichText::new(title).strong()).truncate());
            let artist = entry.map(|e| e.artist.as_str()).unwrap_or_default();
            ui.add(egui::Label::new(egui::RichText::new(artist).weak()).truncate());
        });
        if ui
            .add(icons::button(icons::skip_back(), ICON))
            .on_hover_text(tr!(ui, "asteria-music-bar-previous"))
            .clicked()
        {
            action = Some(BarAction::Previous);
        }
        if play_pause(ui, state) {
            action = Some(BarAction::Toggle);
        }
        if ui
            .add(icons::button(icons::skip_forward(), ICON))
            .on_hover_text(tr!(ui, "asteria-music-bar-next"))
            .clicked()
        {
            action = Some(BarAction::Next);
        }
        if let Some(a) = seek_ui(ui, state, seeking) {
            action = Some(a);
        }
        let icon = if state.volume <= 0.0 {
            icons::volume_x()
        } else {
            icons::volume_2()
        };
        ui.add(icons::image(ui, icon, ICON));
        let mut volume = state.volume;
        ui.spacing_mut().slider_width = 80.0;
        let slider = ui
            .add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false))
            .on_hover_text(tr!(ui, "asteria-music-bar-volume"));
        if slider.changed() {
            action = Some(BarAction::SetVolume(volume));
        }
        match state.status {
            Status::Buffering => {
                ui.spinner();
                ui.weak(tr!(ui, "asteria-music-bar-buffering"));
            }
            Status::Preparing => {
                ui.spinner();
                ui.weak(tr!(ui, "asteria-music-bar-preparing"));
            }
            _ => {}
        }
    });
    action
}

/// The seek bar with `m:ss / m:ss`; the seek is sent when the drag ends.
fn seek_ui(ui: &mut egui::Ui, state: &PlayerState, seeking: &mut Option<f64>) -> Option<BarAction> {
    let duration = state.duration_ms;
    let shown_ms = match *seeking {
        Some(s) => (s * 1000.0) as u64,
        None => state.position_ms,
    };
    let mut action = None;
    let mut text = format_duration(shown_ms);
    if let Some(d) = duration {
        text = format!("{text} / {}", format_duration(d));
    }
    ui.label(text);
    // Without a known length there is nothing to seek in.
    if let Some(d) = duration.filter(|d| *d > 0) {
        let total = d as f64 / 1000.0;
        let mut value = seeking.unwrap_or(state.position_ms as f64 / 1000.0);
        ui.spacing_mut().slider_width = (ui.available_width() - 160.0).clamp(80.0, 600.0);
        let slider = ui.add(egui::Slider::new(&mut value, 0.0..=total).show_value(false));
        if slider.dragged() {
            *seeking = Some(value);
        } else if slider.drag_stopped() || (slider.changed() && seeking.is_none()) {
            *seeking = None;
            action = Some(BarAction::Seek((value * 1000.0) as u64));
        }
    }
    action
}
