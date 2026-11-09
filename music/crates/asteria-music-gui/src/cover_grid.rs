//! A grid of albums: the cover (or a music note while it loads or when there is none), the
//! title and the album artist under it. A click opens the album; the right-click menu saves it
//! for offline (or removes it), and a saved album is marked.
//!
//! Like the client crate's `ThumbGrid` it draws only the rows on screen and asks for only their
//! covers, and it reports the last item drawn (to know when to fetch the next page) and the
//! number of columns, which [`ThumbGrid::columns_for`] works out.

use asteria_common_client::images::{ImageLoader, ImageState, Preset};
use asteria_common_client::offline::{OfflineHandle, PinStatus, PinTarget};
use asteria_common_client::widgets::offline_toggle::saved_mark;
use asteria_common_client::widgets::thumb_grid::{DEFAULT_CELL_SIZE, SPACING, ThumbGrid};
use asteria_music_api::{AlbumId, AlbumSummary};
use corundum_framework::egui::{self, Align2, Color32, Rect, Sense, vec2};
use corundum_framework::gui::{icons, tr};

use crate::offline::PIN_ALBUM;

/// The height under the cover for the title and the artist.
const CAPTION_H: f32 = 40.0;

/// What happened in one frame of the grid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoverGridResponse {
    /// An album was clicked (a double click opens it just the same).
    pub opened: Option<AlbumId>,
    /// How many cells fit in a row at this width.
    pub columns: usize,
    /// The index of the last album in the rows drawn this frame; `None` if none were.
    pub last_visible: Option<usize>,
}

pub struct CoverGrid<'a> {
    albums: &'a [AlbumSummary],
    cell: f32,
    id_salt: egui::IdSalt,
    offline: Option<&'a OfflineHandle>,
}

impl<'a> CoverGrid<'a> {
    pub fn new(albums: &'a [AlbumSummary]) -> Self {
        CoverGrid {
            albums,
            cell: DEFAULT_CELL_SIZE,
            id_salt: egui::IdSalt::new("asteria-cover-grid"),
            offline: None,
        }
    }

    /// The library's copies saved for offline: the menu and the marks need them.
    pub fn offline(mut self, handle: Option<&'a OfflineHandle>) -> Self {
        self.offline = handle;
        self
    }

    /// Tells two grids in one `Ui` apart.
    pub fn id_salt(mut self, salt: impl egui::AsIdSalt) -> Self {
        self.id_salt = egui::IdSalt::new(salt);
        self
    }

    /// The grid as the whole scrolling area.
    pub fn show(self, ui: &mut egui::Ui, images: &mut ImageLoader) -> CoverGridResponse {
        ui.scope(|ui| self.show_inner(ui, images)).inner
    }

    fn show_inner(self, ui: &mut egui::Ui, images: &mut ImageLoader) -> CoverGridResponse {
        let avail = ui.available_width() - ui.spacing().scroll.allocated_width();
        let columns = ThumbGrid::columns_for(avail, self.cell);
        let rows = self.albums.len().div_ceil(columns);
        let mut resp = CoverGridResponse {
            columns,
            ..Default::default()
        };
        ui.spacing_mut().item_spacing = vec2(SPACING, SPACING);
        egui::ScrollArea::vertical()
            .id_salt(self.id_salt)
            .auto_shrink([false, false])
            .show_rows(ui, self.cell + CAPTION_H, rows, |ui, range| {
                for row in range {
                    ui.horizontal(|ui| {
                        let start = row * columns;
                        let end = (start + columns).min(self.albums.len());
                        resp.last_visible = resp.last_visible.max(Some(end - 1));
                        for album in &self.albums[start..end] {
                            if cell(ui, album, self.cell, images, self.offline).clicked() {
                                resp.opened = Some(album.id.clone());
                            }
                        }
                    });
                }
            });
        resp
    }

    /// The grid wrapping inside the caller's scrolling `Ui`, for the few albums of an artist.
    pub fn show_flowing(self, ui: &mut egui::Ui, images: &mut ImageLoader) -> Option<AlbumId> {
        let mut opened = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(SPACING, SPACING);
            for album in self.albums {
                if cell(ui, album, self.cell, images, self.offline).clicked() {
                    opened = Some(album.id.clone());
                }
            }
        });
        opened
    }
}

/// The cover of `album`, drawn into `rect` (a square): the picture, or a music note.
pub fn paint_cover(ui: &egui::Ui, rect: Rect, album: &AlbumSummary, images: &mut ImageLoader) {
    let visuals = ui.visuals();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    let ready = album
        .cover_item
        .as_ref()
        .map(|id| images.request_id(id, Preset::Thumb));
    paint_picture(ui, rect, ready.as_ref());
}

/// `picture` drawn to fit `rect` if it is ready, else a music note (`rect` is already filled).
pub fn paint_picture(ui: &egui::Ui, rect: Rect, picture: Option<&ImageState>) {
    let visuals = ui.visuals();
    let painter = ui.painter_at(rect);
    if let Some(ImageState::Ready(tex)) = picture {
        let size = tex.size_vec2();
        let scale = (rect.width() / size.x.max(1.0)).min(rect.height() / size.y.max(1.0));
        painter.image(
            tex.id(),
            Rect::from_center_size(rect.center(), size * scale),
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        let note = Rect::from_center_size(rect.center(), rect.size() * 0.4);
        icons::paint(ui, note, icons::music(), visuals.weak_text_color());
    }
}

/// The right-click menu of what can be saved for offline (`kind` and `id` as pinned, `name` for
/// the Offline page): "Save for offline", or "Remove from offline" once pinned. Nothing without
/// a `handle`.
pub fn offline_menu(
    response: &egui::Response,
    handle: Option<&OfflineHandle>,
    kind: &str,
    id: &str,
    name: &str,
) {
    let Some(handle) = handle else {
        return;
    };
    response.context_menu(|ui| {
        if handle.status(kind, id).is_some() {
            if ui.button(tr!(ui, "asteria-music-offline-remove")).clicked() {
                handle.unpin(kind, id);
                ui.close();
            }
        } else if ui.button(tr!(ui, "asteria-music-offline-save")).clicked() {
            handle.pin(PinTarget {
                kind: kind.into(),
                id: id.into(),
                name: name.into(),
            });
            ui.close();
        }
    });
}

/// Whether `kind`/`id` is saved for offline (pinned, and its copies made).
pub fn is_saved(handle: Option<&OfflineHandle>, kind: &str, id: &str) -> bool {
    handle.is_some_and(|h| h.status(kind, id) == Some(PinStatus::Saved))
}

fn cell(
    ui: &mut egui::Ui,
    album: &AlbumSummary,
    size: f32,
    images: &mut ImageLoader,
    offline: Option<&OfflineHandle>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size + CAPTION_H), Sense::click());
    let label = format!("{} - {}", album.title, album.album_artist);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label.as_str()));
    offline_menu(&response, offline, PIN_ALBUM, &album.id.0, &album.title);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let cover = Rect::from_min_size(rect.min, vec2(size, size));
    paint_cover(ui, cover, album, images);
    if is_saved(offline, PIN_ALBUM, &album.id.0) {
        // In a child of its own at the cover's corner, so the row's layout does not move.
        let corner = Rect::from_min_size(cover.right_top() + vec2(-22.0, 4.0), vec2(18.0, 18.0));
        saved_mark(&mut ui.new_child(egui::UiBuilder::new().max_rect(corner)));
    }
    let visuals = ui.visuals();
    if response.hovered() {
        ui.painter_at(rect).rect_stroke(
            cover.shrink(1.0),
            4.0,
            visuals.widgets.hovered.bg_stroke,
            egui::StrokeKind::Inside,
        );
    }
    let font = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::TextStyle::Small.resolve(ui.style());
    let painter = ui.painter_at(rect);
    let lines = [
        (&album.title, font, visuals.text_color()),
        (&album.album_artist, small, visuals.weak_text_color()),
    ];
    let mut y = cover.bottom() + 3.0;
    for (text, font, color) in lines {
        let line = Rect::from_min_size(
            egui::pos2(rect.left(), y),
            vec2(size, CAPTION_H / 2.0 - 2.0),
        );
        painter.with_clip_rect(line).text(
            line.left_center(),
            Align2::LEFT_CENTER,
            text,
            font,
            color,
        );
        y += CAPTION_H / 2.0 - 2.0;
    }
    response
}
