//! The playback bar at the foot of every screen: the cover, the title and artist, the
//! transport buttons, the seek bar, the volume, and what is wrong, if anything.
//!
//! Wide, it has everything; narrow (a phone) it is one row of cover, title and play/pause.
//! It draws nothing for an empty queue. It only reports what the user asked for
//! ([`BarAction`]); the app turns that into a [`Command`](crate::player::Command).
//!
//! The side pane beside every screen shows the same track, larger, with its rating to click
//! ([`side`]).

use asteria_common_client::images::{ImageLoader, Preset};
use asteria_common_client::widgets::stars::stars;
use asteria_music_api::MediaItem;
use corundum_framework::egui::{self, Align, Layout, Sense, vec2};
use corundum_framework::gui::{icons, tr};

use crate::cover_grid::paint_picture;
use crate::player::{PlayerError, PlayerState, Status};
use crate::tracks::format_duration;

const COVER: f32 = 48.0;
const COVER_COMPACT: f32 = 36.0;
const ICON: f32 = 20.0;
/// The largest the side pane draws the cover.
const COVER_SIDE: f32 = 240.0;
/// The side pane's stars.
const STAR_SIDE: f32 = 18.0;

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

/// What the side pane says: the current track's title, artists and album (either may be
/// empty), or `None` when nothing is current.
pub(crate) fn side_text(state: &PlayerState) -> Option<(&str, &str, &str)> {
    current(state).map(|e| (e.title.as_str(), e.artist.as_str(), e.album.as_str()))
}

/// The current track's item, the one the side pane rates.
pub(crate) fn current_media(state: &PlayerState) -> Option<&MediaItem> {
    current(state).map(|e| &e.media)
}

/// Fills the side pane: the current track's cover, title, artists, album and `rating` as
/// stars, or a word that nothing is playing. Returns the rating a click on a star asks for;
/// with `can_rate` false the stars are drawn disabled and never do.
pub(crate) fn side(
    ui: &mut egui::Ui,
    state: &PlayerState,
    images: Option<&mut ImageLoader>,
    rating: Option<i8>,
    can_rate: bool,
) -> Option<i8> {
    let Some((title, artist, album)) = side_text(state) else {
        ui.weak(tr!(ui, "asteria-music-side-nothing"));
        return None;
    };
    let size = ui.available_width().min(COVER_SIDE);
    cover(ui, state, images, size);
    ui.add_space(8.0);
    ui.heading(title);
    if !artist.is_empty() {
        ui.label(artist);
    }
    if !album.is_empty() {
        ui.weak(album);
    }
    ui.add_space(4.0);
    ui.add_enabled_ui(can_rate, |ui| stars(ui, rating, STAR_SIDE))
        .inner
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::entry;

    fn state(queue: Vec<crate::player::QueueEntry>, current: Option<usize>) -> PlayerState {
        PlayerState {
            queue,
            current,
            status: Status::Paused,
            position_ms: 0,
            duration_ms: None,
            volume: 1.0,
            error: None,
        }
    }

    #[test]
    fn the_side_pane_shows_the_current_track() {
        let mut b = entry("b");
        b.artist = "Artist".into();
        b.album = "Album".into();
        let s = state(vec![entry("a"), b], Some(1));
        assert_eq!(side_text(&s), Some(("b", "Artist", "Album")));
    }

    /// Draws the side pane for `s` with nothing rated, clicking at `click` if given; returns
    /// the stars' centres and what the pane returned.
    fn side_frame(
        ctx: &egui::Context,
        s: &PlayerState,
        can_rate: bool,
        events: Vec<egui::Event>,
    ) -> (Vec<egui::Pos2>, Option<i8>) {
        let mut clicked = None;
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    vec2(300.0, 500.0),
                )),
                events,
                ..Default::default()
            },
            |ui| clicked = side(ui, s, None, None, can_rate),
        );
        // There is no renderer to hand the texture uploads to.
        out.textures_delta.clear();
        let mut stars: Vec<egui::Pos2> = out
            .platform_output
            .accesskit_update
            .map(|u| u.nodes)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, n)| n.label().is_some_and(|l| l.starts_with("Rate ")))
            .filter_map(|(_, n)| n.bounds())
            .map(|b| egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32))
            .collect();
        stars.sort_by(|a, b| a.x.total_cmp(&b.x));
        (stars, clicked)
    }

    /// Whether a click on the side pane's third star asks for a rating.
    fn third_star_rates(can_rate: bool) -> Option<i8> {
        let ctx = egui::Context::default();
        crate::i18n::register(&ctx);
        ctx.enable_accesskit();
        let s = state(vec![entry("a")], Some(0));
        let (stars, _) = side_frame(&ctx, &s, can_rate, vec![]);
        assert_eq!(stars.len(), 5, "{stars:?}");
        let at = stars[2];
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut clicked = None;
        for events in [
            vec![egui::Event::PointerMoved(at)],
            vec![press(true)],
            vec![press(false)],
        ] {
            clicked = clicked.or(side_frame(&ctx, &s, can_rate, events).1);
        }
        clicked
    }

    #[test]
    fn the_side_pane_rates_only_when_it_may() {
        assert_eq!(third_star_rates(true), Some(3));
        // Once the session has failed, the stars are there but do nothing.
        assert_eq!(third_star_rates(false), None);
    }

    #[test]
    fn with_nothing_current_the_side_pane_says_so() {
        assert_eq!(side_text(&state(vec![], None)), None);
        assert_eq!(side_text(&state(vec![entry("a")], None)), None);
    }
}
