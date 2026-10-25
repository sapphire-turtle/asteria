//! A scrolling grid of thumbnails that only draws, and only asks for, the rows on screen.
//!
//! ```ignore
//! let r = ThumbGrid::new(&items)
//!     .cell_size(160.0)
//!     .selected(selected.clone())   // the app owns the selection
//!     .show(ui, session.images());
//! if let Some(id) = r.selected { selected = Some(id); }
//! if let Some(id) = r.double_clicked { /* open it */ }
//! ```
//!
//! The grid does no I/O: it asks a [`ThumbSource`] (the [`ImageLoader`] of the session) for
//! the state of each visible thumbnail, every frame.
//!
//! Keyboard: the arrow keys move the selection by one (left, right) or by a row (up, down), and
//! Enter reports the selection as double-clicked. The grid owns the keyboard while no widget
//! has focus (`ctx.memory(|m| m.focused())` is `None`): a focused text field or button keeps its
//! keys, and so does egui's focus navigation. A key is consumed only when the grid acts on it:
//! Enter with nothing selected is left alone. A move scrolls the selected cell into view. With
//! nothing selected, an arrow selects the first item. The Lucide icons need `corundum_framework::gui::icons::install_loaders(ctx)` once;
//! the shell does it.

use asteria_framework_api::{ItemId, MediaItem, MediaKind};
use corundum_framework::egui::{self, Color32, Rect, Sense, vec2};
use corundum_framework::gui::{icons, tr};

use crate::images::{ImageLoader, ImageState, Preset};

/// The gap between cells, in points, both ways.
pub const SPACING: f32 = 6.0;
/// The cell edge when none is given.
pub const DEFAULT_CELL_SIZE: f32 = 160.0;
/// The most stars drawn on a cell.
const MAX_STARS: i8 = 5;

/// Where the grid gets thumbnails: asked for every visible item, every frame.
pub trait ThumbSource {
    fn thumb(&mut self, item: &MediaItem) -> ImageState;
}

impl ThumbSource for ImageLoader {
    fn thumb(&mut self, item: &MediaItem) -> ImageState {
        self.request(item, Preset::Thumb)
    }
}

/// What happened in one frame of the grid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThumbGridResponse {
    /// A cell was clicked (also reported for the first click of a double click).
    pub clicked: Option<ItemId>,
    /// A cell was double-clicked, or Enter was pressed with a selection.
    pub double_clicked: Option<ItemId>,
    /// The selection after a click or an arrow key moved it; `None` if it did not move.
    pub selected: Option<ItemId>,
    /// How many cells fit in a row at this width.
    pub columns: usize,
    /// The index of the last item in the rows drawn this frame; `None` if none were. When it
    /// nears the end of the items, it is time to fetch the next page.
    pub last_visible: Option<usize>,
}

pub struct ThumbGrid<'a> {
    items: &'a [MediaItem],
    cell: f32,
    selected: Option<ItemId>,
    id_salt: egui::IdSalt,
}

/// What the grid remembers between frames, to scroll a moved selection into view.
#[derive(Clone, Copy, Default)]
struct View {
    offset: f32,
    height: f32,
}

impl<'a> ThumbGrid<'a> {
    pub fn new(items: &'a [MediaItem]) -> Self {
        Self {
            items,
            cell: DEFAULT_CELL_SIZE,
            selected: None,
            id_salt: egui::IdSalt::new("asteria-thumb-grid"),
        }
    }

    pub fn cell_size(mut self, cell: f32) -> Self {
        self.cell = cell.max(24.0);
        self
    }

    /// The selected item, owned by the caller.
    pub fn selected(mut self, selected: Option<ItemId>) -> Self {
        self.selected = selected;
        self
    }

    /// Tells two grids in one `Ui` apart (their scroll position and view state).
    pub fn id_salt(mut self, salt: impl egui::AsIdSalt) -> Self {
        self.id_salt = egui::IdSalt::new(salt);
        self
    }

    /// `max(1, floor((width + SPACING) / (cell + SPACING)))`.
    pub fn columns_for(width: f32, cell: f32) -> usize {
        (((width + SPACING) / (cell + SPACING)).floor() as usize).max(1)
    }

    pub fn show(self, ui: &mut egui::Ui, source: &mut impl ThumbSource) -> ThumbGridResponse {
        // A scope, so the spacing set below does not leak into the caller's `Ui`.
        ui.scope(|ui| self.show_inner(ui, source)).inner
    }

    fn show_inner(self, ui: &mut egui::Ui, source: &mut impl ThumbSource) -> ThumbGridResponse {
        let cell = self.cell;
        let avail = ui.available_width() - ui.spacing().scroll.allocated_width();
        let columns = Self::columns_for(avail, cell);
        let rows = self.items.len().div_ceil(columns);
        let row_h = cell + SPACING;
        let view_id = ui.make_persistent_id(self.id_salt);
        let view: View = ui.data(|d| d.get_temp(view_id)).unwrap_or_default();
        let mut resp = ThumbGridResponse {
            columns,
            ..Default::default()
        };
        let mut scroll_to = None;

        let keys_free = ui.ctx().memory(|m| m.focused()).is_none();
        if keys_free && !self.items.is_empty() {
            use egui::Key::*;
            let pressed = |ui: &egui::Ui, k| ui.input(|i| i.key_pressed(k));
            let arrow = [ArrowLeft, ArrowRight, ArrowUp, ArrowDown]
                .into_iter()
                .find(|k| pressed(ui, *k));
            let enter = pressed(ui, Enter);
            // The position is looked up only when a key may act on it.
            let current = (arrow.is_some() || enter)
                .then(|| {
                    self.selected
                        .as_ref()
                        .and_then(|id| self.items.iter().position(|i| &i.id == id))
                })
                .flatten();
            let last = self.items.len() - 1;
            let moved = match (arrow, current) {
                (Some(_), None) => Some(0),
                (Some(ArrowLeft), Some(i)) => Some(i.saturating_sub(1)),
                (Some(ArrowRight), Some(i)) => Some((i + 1).min(last)),
                (Some(ArrowUp), Some(i)) => Some(i.saturating_sub(columns)),
                (Some(_), Some(i)) => Some((i + columns).min(last)),
                (None, _) => None,
            };
            // Take only the key that did something.
            let consume = |ui: &egui::Ui, k| {
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
            };
            if let (Some(k), Some(_)) = (arrow, moved) {
                consume(ui, k);
            }
            if let Some(i) = moved {
                let id = self.items[i].id.clone();
                if current != Some(i) {
                    resp.selected = Some(id);
                }
                // Keep the cell fully visible: only moves the view if it is not.
                let (top, bottom) = (
                    (i / columns) as f32 * row_h,
                    (i / columns) as f32 * row_h + cell,
                );
                if view.height > 0.0 {
                    if top < view.offset {
                        scroll_to = Some(top);
                    } else if bottom > view.offset + view.height {
                        scroll_to = Some(bottom - view.height);
                    }
                }
            } else if enter && let Some(id) = current.map(|i| self.items[i].id.clone()) {
                consume(ui, Enter);
                resp.double_clicked = Some(id);
            }
        }
        let selected = resp.selected.clone().or(self.selected.clone());

        ui.spacing_mut().item_spacing = vec2(SPACING, SPACING);
        let mut area = egui::ScrollArea::vertical()
            .id_salt(self.id_salt)
            .auto_shrink([false, false]);
        if let Some(offset) = scroll_to {
            area = area.vertical_scroll_offset(offset.max(0.0));
        }
        let out = area.show_rows(ui, cell, rows, |ui, range| {
            for row in range {
                ui.horizontal(|ui| {
                    let start = row * columns;
                    let end = (start + columns).min(self.items.len());
                    resp.last_visible = resp.last_visible.max(Some(end - 1));
                    for item in &self.items[start..end] {
                        let is_selected = selected.as_ref() == Some(&item.id);
                        let r = draw_cell(ui, item, cell, is_selected, source);
                        if r.clicked() {
                            resp.clicked = Some(item.id.clone());
                            if !is_selected {
                                resp.selected = Some(item.id.clone());
                            }
                        }
                        if r.double_clicked() {
                            resp.double_clicked = Some(item.id.clone());
                        }
                    }
                });
            }
        });
        ui.data_mut(|d| {
            d.insert_temp(
                view_id,
                View {
                    offset: out.state.clamped_offset().y,
                    height: out.inner_rect.height(),
                },
            )
        });
        resp
    }
}

/// Lucide's `lock` (ISC, `assets/icons/LICENSE-Lucide`), stroked in white like corundum's icons
/// so it tints the same way. Move it to corundum when corundum gets a lock icon.
fn lock() -> egui::ImageSource<'static> {
    egui::include_image!("../../assets/icons/lock.svg")
}

fn draw_cell(
    ui: &mut egui::Ui,
    item: &MediaItem,
    cell: f32,
    selected: bool,
    source: &mut impl ThumbSource,
) -> egui::Response {
    let state = source.thumb(item);
    let (rect, response) = ui.allocate_exact_size(vec2(cell, cell), Sense::click());
    let name = item.path.rsplit('/').next().unwrap_or(&item.path);
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, name));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.visuals();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    match &state {
        ImageState::Ready(tex) => {
            let size = tex.size_vec2();
            let scale = (cell / size.x.max(1.0)).min(cell / size.y.max(1.0));
            let image = Rect::from_center_size(rect.center(), size * scale);
            painter.image(
                tex.id(),
                image,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        ImageState::Loading => {}
        ImageState::Failed(_) => {
            let r = Rect::from_center_size(rect.center(), vec2(24.0, 24.0));
            icons::paint(ui, r, icons::cloud_off(), visuals.weak_text_color());
            let ctx = ui.ctx().clone();
            response.clone().on_hover_ui(|ui| {
                ui.label(tr!(ctx, "asteria-client-grid-failed"));
            });
        }
    }
    let badge = Color32::from_black_alpha(150);
    let icon_size = (cell * 0.14).clamp(12.0, 20.0);
    // The overlays sit on a dark chip so they read on any picture.
    let chip = |rect: Rect, src: egui::ImageSource<'static>| {
        painter.rect_filled(rect.expand(2.0), 3.0, badge);
        icons::paint(ui, rect, src, Color32::WHITE);
    };
    let pad = 4.0;
    if item.kind == MediaKind::Video {
        let r = Rect::from_min_size(
            rect.left_top() + vec2(pad, pad) + vec2(2.0, 2.0),
            vec2(icon_size, icon_size),
        );
        chip(r, icons::play());
        if ui.rect_contains_pointer(r) {
            let ctx = ui.ctx().clone();
            response.clone().on_hover_ui(|ui| {
                ui.label(tr!(ctx, "asteria-client-grid-video"));
            });
        }
    }
    if let Some(reason) = &item.read_only_reason {
        let r = Rect::from_min_size(
            rect.right_top() + vec2(-pad - icon_size - 2.0, pad + 2.0),
            vec2(icon_size, icon_size),
        );
        chip(r, lock());
        if ui.rect_contains_pointer(r) {
            response.clone().on_hover_text(tr!(
                ui,
                "asteria-client-grid-read-only",
                reason = reason.as_str()
            ));
        }
    }
    if let Some(stars) = item.rating.filter(|r| *r > 0).map(|r| r.min(MAX_STARS)) {
        let star = icon_size * 0.8;
        let width = star * f32::from(stars as u8);
        let bar = Rect::from_min_size(
            rect.left_bottom() + vec2(pad + 2.0, -pad - 2.0 - star),
            vec2(width, star),
        );
        painter.rect_filled(bar.expand(2.0), 3.0, badge);
        for n in 0..stars {
            let r = Rect::from_min_size(
                bar.min + vec2(star * f32::from(n as u8), 0.0),
                vec2(star, star),
            );
            icons::paint(ui, r, icons::star(), Color32::from_rgb(255, 205, 60));
        }
        if ui.rect_contains_pointer(bar) {
            response.clone().on_hover_text(tr!(
                ui,
                "asteria-client-grid-rating",
                stars = u64::from(stars as u8)
            ));
        }
    }
    if selected {
        painter.rect_stroke(
            rect.shrink(1.0),
            4.0,
            egui::Stroke::new(2.0, visuals.selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    } else if response.hovered() {
        painter.rect_stroke(
            rect.shrink(1.0),
            4.0,
            visuals.widgets.hovered.bg_stroke,
            egui::StrokeKind::Inside,
        );
    }
    response
}
