//! The five-star rating widget the apps share.

use corundum_framework::egui;
use corundum_framework::gui::theme::{Swatch, Tone};
use corundum_framework::gui::{icons, tr};

/// The most stars.
pub const MAX_RATING: i8 = 5;

/// The rating a click on star `star` (1 to 5) makes: that many stars, or 0 (none) when
/// `star` is the rating already set.
pub fn rating_after_click(current: Option<i8>, star: i8) -> i8 {
    if current == Some(star) { 0 } else { star }
}

/// Five stars at `size` points; returns the rating a click asks for. A disabled `ui` draws
/// them but never returns one.
pub fn stars(ui: &mut egui::Ui, rating: Option<i8>, size: f32) -> Option<i8> {
    let set = rating.unwrap_or(0);
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for star in 1..=MAX_RATING {
            let (tone, swatch) = if star <= set {
                (Tone::Text, Swatch::Yellow)
            } else {
                (Tone::Border, Swatch::Neutral)
            };
            let image = icons::image_as(ui, icons::star(), size, tone, swatch);
            let hover = if rating == Some(star) {
                tr!(ui, "asteria-common-client-rating-clear")
            } else {
                tr!(
                    ui,
                    "asteria-common-client-rating-set",
                    stars = i64::from(star)
                )
            };
            if ui
                .add(egui::Button::image(image).frame(false))
                .on_hover_text(hover)
                .clicked()
            {
                clicked = Some(rating_after_click(rating, star));
            }
        }
    });
    clicked
}
