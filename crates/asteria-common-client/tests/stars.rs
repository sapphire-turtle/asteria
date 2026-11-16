//! The shared star widget: what a click asks for, drawn headlessly with
//! `egui::Context::run_ui` and clicked at a star's centre.

use asteria_common_client::i18n;
use asteria_common_client::widgets::stars::{MAX_RATING, rating_after_click, stars};
use corundum_framework::egui;
use corundum_framework::gui::icons;

#[test]
fn rating_clicks_set_or_clear() {
    assert_eq!(rating_after_click(None, 3), 3);
    assert_eq!(rating_after_click(Some(0), 1), 1);
    assert_eq!(rating_after_click(Some(3), 3), 0);
    assert_eq!(rating_after_click(Some(3), 5), 5);
    assert_eq!(MAX_RATING, 5);
}

/// One frame of five stars with `events`; what they returned and the stars' centres, left to
/// right.
fn frame(
    ctx: &egui::Context,
    rating: Option<i8>,
    enabled: bool,
    events: Vec<egui::Event>,
) -> (Option<i8>, Vec<egui::Pos2>) {
    let mut asked = None;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            ui.add_enabled_ui(enabled, |ui| {
                asked = stars(ui, rating, 18.0);
            });
        },
    );
    output.textures_delta.clear();
    let mut centres = Vec::new();
    if let Some(update) = output.platform_output.accesskit_update {
        for (_, node) in update.nodes {
            if node.role() != egui::accesskit::Role::Button {
                continue;
            }
            if let Some(b) = node.bounds() {
                centres.push(egui::pos2(
                    ((b.x0 + b.x1) / 2.0) as f32,
                    ((b.y0 + b.y1) / 2.0) as f32,
                ));
            }
        }
    }
    centres.sort_by(|a, b| a.x.total_cmp(&b.x));
    (asked, centres)
}

/// Clicks the `star`th star (1 to 5); what the widget returned over the frames.
fn click(rating: Option<i8>, enabled: bool, star: usize) -> Option<i8> {
    let ctx = egui::Context::default();
    i18n::register(&ctx);
    icons::install_loaders(&ctx);
    ctx.enable_accesskit();
    let (_, centres) = frame(&ctx, rating, enabled, Vec::new());
    assert_eq!(centres.len(), 5, "{centres:?}");
    let at = centres[star - 1];
    let press = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    let mut asked = None;
    for events in [
        vec![egui::Event::PointerMoved(at)],
        vec![press(true)],
        vec![press(false)],
    ] {
        asked = asked.or(frame(&ctx, rating, enabled, events).0);
    }
    asked
}

#[test]
fn a_click_on_star_four_asks_for_four() {
    assert_eq!(click(None, true, 4), Some(4));
}

#[test]
fn a_click_on_the_rating_set_asks_for_none() {
    assert_eq!(click(Some(4), true, 4), Some(0));
}

#[test]
fn disabled_stars_never_ask() {
    assert_eq!(click(None, false, 4), None);
}
