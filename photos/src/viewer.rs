//! The full-screen preview of the item opened from the grid (double-click, or Enter).
//!
//! Left and Right move to the previous and next item of the listing (the selection follows,
//! and the next page is fetched near the end); Esc closes it. Videos show their still
//! preview only: playback is for a later plan. It draws the `preview` preset, and asks for
//! the neighbours' too so a step shows at once: three previews, well within the loader's
//! budget of eight.

use asteria_framework_api::MediaKind;
use asteria_framework_client::errors::error_text;
use asteria_framework_client::images::{ImageState, Preset};
use asteria_framework_client::session::Session;
use corundum_framework::egui;
use corundum_framework::gui::{icons, tr};

use crate::browse::BrowseState;
use crate::detail::fit;

/// What the person asked of the viewer, besides moving in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Asked {
    Nothing,
    /// Close it and show the item's details in the side pane.
    Details,
}

/// Draw the viewer for `browse`'s opened item, filling `ui`. Closes itself (clearing the
/// opened item) on Esc, on its close button, or when the item is no longer listed.
pub(crate) fn ui(ui: &mut egui::Ui, browse: &mut BrowseState, session: &mut Session) -> Asked {
    let (prev, next, close) = ui.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
        )
    });
    if close {
        browse.close_viewer();
        ui.ctx().request_repaint();
        return Asked::Nothing;
    }
    if prev {
        browse.step(session, -1);
    }
    if next {
        browse.step(session, 1);
    }
    let Some(index) = browse.opened_index() else {
        browse.close_viewer();
        ui.ctx().request_repaint();
        return Asked::Nothing;
    };
    let total = browse.items().len();
    let item = browse.items()[index].clone();
    let mut asked = Asked::Nothing;
    let mut step = 0;
    let fill = ui.visuals().extreme_bg_color;
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(fill).inner_margin(8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(icons::button(icons::x(), 16.0))
                    .on_hover_text(tr!(ui, "asteria-photos-viewer-close"))
                    .clicked()
                {
                    browse.close_viewer();
                }
                let name = item.path.rsplit('/').next().unwrap_or(&item.path);
                ui.strong(name);
                ui.weak(tr!(
                    ui,
                    "asteria-photos-viewer-position",
                    index = index + 1,
                    total = total
                ));
                if item.kind == MediaKind::Video {
                    ui.weak(tr!(ui, "asteria-photos-detail-video"));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let info = icons::image(ui, icons::info(), 16.0);
                    if ui
                        .add(egui::Button::image_and_text(
                            info,
                            tr!(ui, "asteria-photos-viewer-details"),
                        ))
                        .clicked()
                    {
                        asked = Asked::Details;
                        browse.close_viewer();
                    }
                });
            });
            let area = ui.available_rect_before_wrap();
            match session.images().request(&item, Preset::Preview) {
                ImageState::Ready(tex) => {
                    let size = fit(tex.size_vec2(), area.size());
                    let rect = egui::Rect::from_center_size(area.center(), size);
                    egui::Image::new(egui::load::SizedTexture::new(tex.id(), size))
                        .paint_at(ui, rect);
                }
                ImageState::Loading => {
                    let rect = egui::Rect::from_center_size(area.center(), egui::vec2(32.0, 32.0));
                    ui.put(rect, egui::Spinner::new().size(32.0));
                }
                ImageState::Failed(e) => {
                    let reason = error_text(ui.ctx(), &e);
                    let rect = egui::Rect::from_center_size(area.center(), area.size() * 0.8);
                    ui.put(
                        rect,
                        egui::Label::new(tr!(
                            ui,
                            "asteria-photos-detail-preview-failed",
                            reason = reason
                        )),
                    );
                }
            }
            // The neighbours, so a step shows at once.
            for i in [index.checked_sub(1), Some(index + 1)]
                .into_iter()
                .flatten()
            {
                if let Some(n) = browse.items().get(i) {
                    let n = n.clone();
                    session.images().request(&n, Preset::Preview);
                }
            }
            let side = egui::vec2(40.0, 40.0);
            let left = egui::Rect::from_min_size(
                egui::pos2(area.left(), area.center().y - side.y / 2.0),
                side,
            );
            let right = egui::Rect::from_min_size(
                egui::pos2(area.right() - side.x, area.center().y - side.y / 2.0),
                side,
            );
            if index > 0 {
                let back = icons::image(ui, icons::chevron_right(), 24.0)
                    .rotate(std::f32::consts::PI, egui::Vec2::splat(0.5));
                if ui
                    .put(left, egui::Button::image(back))
                    .on_hover_text(tr!(ui, "asteria-photos-viewer-previous"))
                    .clicked()
                {
                    step = -1;
                }
            }
            if index + 1 < total {
                let on = icons::image(ui, icons::chevron_right(), 24.0);
                if ui
                    .put(right, egui::Button::image(on))
                    .on_hover_text(tr!(ui, "asteria-photos-viewer-next"))
                    .clicked()
                {
                    step = 1;
                }
            }
        });
    if step != 0 {
        browse.step(session, step);
    }
    asked
}
