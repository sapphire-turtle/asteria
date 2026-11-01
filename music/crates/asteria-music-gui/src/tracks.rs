//! The table of an album's or a playlist's tracks, drawn row by row (only the rows on screen).
//!
//! Columns: disc, number, title, the artist (only when it differs from the album artist), and
//! the length as `m:ss`. In a playlist a [`Missing`](EntryStatus::Missing) entry is dimmed, with
//! the path written in the playlist, and a [`Moved`](EntryStatus::Moved) one is marked. A click
//! on a row plays from it; the right-click menu has "play next" and "add to queue". The table
//! only reports what the user asked for ([`TrackAction`]); the app turns it into a command.

use asteria_music_api::{Album, EntryStatus, MusicTrack, Playlist};
use corundum_framework::egui::{self, Align2, Sense, vec2};
use corundum_framework::gui::tr;

/// What the user asked of a row. The index is the row's: an album's track, a playlist's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackAction {
    Play(usize),
    PlayNext(usize),
    Append(usize),
}

/// One row of the table.
pub struct Row<'a> {
    pub status: EntryStatus,
    pub track: Option<&'a MusicTrack>,
    /// The path as written in the playlist, for an entry that is not found.
    pub raw_path: Option<&'a str>,
    pub hint: Option<&'a str>,
}

/// The rows of an album: every track, found.
pub fn album_rows(album: &Album) -> Vec<Row<'_>> {
    album
        .tracks
        .iter()
        .map(|t| Row {
            status: EntryStatus::Found,
            track: Some(t),
            raw_path: None,
            hint: None,
        })
        .collect()
}

/// The rows of a playlist: every entry, in order.
pub fn playlist_rows(playlist: &Playlist) -> Vec<Row<'_>> {
    playlist
        .entries
        .iter()
        .map(|e| Row {
            status: e.status,
            track: e.track.as_ref(),
            raw_path: Some(e.raw_path.as_str()),
            hint: e.title_hint.as_deref(),
        })
        .collect()
}

/// `m:ss` (minutes are not wrapped into hours: `75:00`).
pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The artists of `track` joined with ", ", unless they are the album artist (or there are
/// none), in which case the table leaves the cell empty.
pub fn artist_cell(track: &MusicTrack, album_artist: Option<&str>) -> Option<String> {
    let artists = track.tags.artists.join(", ");
    if artists.is_empty() {
        return None;
    }
    let same = album_artist.is_some_and(|a| a == artists)
        || track
            .tags
            .album_artist
            .as_deref()
            .is_some_and(|a| a == artists);
    (!same).then_some(artists)
}

/// Whether the row can be played (it has a track that was found).
fn playable(row: &Row<'_>) -> bool {
    row.status != EntryStatus::Missing && row.track.is_some()
}

const DISC_W: f32 = 44.0;
const NUMBER_W: f32 = 36.0;
const LENGTH_W: f32 = 56.0;
const PAD: f32 = 6.0;

/// Draws the table. `album_artist` is the album's artist, if the rows all belong to one album.
/// Returns what the user asked for this frame.
pub fn show(
    ui: &mut egui::Ui,
    salt: impl egui::AsIdSalt,
    rows: &[Row<'_>],
    album_artist: Option<&str>,
) -> Option<TrackAction> {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
    let mut action = None;
    header(ui, &font);
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .auto_shrink([false, false])
        .show_rows(ui, row_h, rows.len(), |ui, range| {
            for index in range {
                let row = &rows[index];
                if let Some(a) = row_ui(ui, index, row, album_artist, &font, row_h) {
                    action = Some(a);
                }
            }
        });
    action
}

/// The column rectangles of a row `rect` wide: disc, number, title, artist, length.
fn columns(rect: egui::Rect) -> [egui::Rect; 5] {
    let rect = rect.shrink2(vec2(PAD, 0.0));
    let flexible = (rect.width() - DISC_W - NUMBER_W - LENGTH_W).max(0.0);
    let title_w = flexible * 0.6;
    let mut x = rect.left();
    let mut next = |w: f32| {
        let r = egui::Rect::from_min_size(egui::pos2(x, rect.top()), vec2(w, rect.height()));
        x += w;
        r
    };
    [
        next(DISC_W),
        next(NUMBER_W),
        next(title_w),
        next(flexible - title_w),
        next(LENGTH_W),
    ]
}

fn header(ui: &mut egui::Ui, font: &egui::FontId) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), font.size + 8.0), Sense::hover());
    let cols = columns(rect);
    let color = ui.visuals().weak_text_color();
    let painter = ui.painter_at(rect);
    let keys = [
        "asteria-music-tracks-disc",
        "asteria-music-tracks-number",
        "asteria-music-tracks-title",
        "asteria-music-tracks-artist",
        "asteria-music-tracks-length",
    ];
    for (i, (col, key)) in cols.iter().zip(keys).enumerate() {
        let text = tr!(ui, key);
        let (align, x) = if i == 4 {
            (Align2::RIGHT_CENTER, col.right() - PAD)
        } else {
            (Align2::LEFT_CENTER, col.left())
        };
        painter.with_clip_rect(*col).text(
            egui::pos2(x, col.center().y),
            align,
            text,
            font.clone(),
            color,
        );
    }
    ui.separator();
}

fn row_ui(
    ui: &mut egui::Ui,
    index: usize,
    row: &Row<'_>,
    album_artist: Option<&str>,
    font: &egui::FontId,
    row_h: f32,
) -> Option<TrackAction> {
    let can_play = playable(row);
    let sense = if can_play {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), sense);
    let tags = row.track.map(|t| &t.tags);
    let title = match (row.track, row.hint, row.raw_path) {
        (Some(t), ..) if row.status != EntryStatus::Missing => t.display_title.clone(),
        (_, Some(hint), _) => hint.to_string(),
        (_, _, Some(path)) => path.to_string(),
        _ => String::new(),
    };
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, can_play, title.as_str()));
    if ui.is_rect_visible(rect) {
        let visuals = ui.visuals();
        if response.hovered() && can_play {
            ui.painter()
                .rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
        }
        let strong = visuals.text_color();
        let weak = visuals.weak_text_color();
        let color = if row.status == EntryStatus::Missing {
            weak
        } else {
            strong
        };
        let cols = columns(rect);
        let painter = ui.painter_at(rect);
        let text = |col: usize, s: String, color: egui::Color32, right: bool| {
            let (align, x) = if right {
                (Align2::RIGHT_CENTER, cols[col].right() - PAD)
            } else {
                (Align2::LEFT_CENTER, cols[col].left())
            };
            painter.with_clip_rect(cols[col]).text(
                egui::pos2(x, cols[col].center().y),
                align,
                s,
                font.clone(),
                color,
            );
        };
        if let Some(tags) = tags {
            if let Some(d) = tags.disc {
                text(0, d.to_string(), weak, false);
            }
            if let Some(n) = tags.track {
                text(1, n.to_string(), weak, false);
            }
            if let Some(ms) = tags.duration_ms {
                text(4, format_duration(ms), weak, true);
            }
        }
        let mut shown_title = title.clone();
        if row.status == EntryStatus::Moved {
            shown_title = format!("{shown_title}  [{}]", tr!(ui, "asteria-music-tracks-moved"));
        }
        text(2, shown_title, color, false);
        match (row.status, row.track) {
            (EntryStatus::Missing, _) => {
                if let Some(path) = row.raw_path.filter(|p| *p != title) {
                    text(3, path.to_string(), weak, false);
                }
            }
            (_, Some(t)) => {
                if let Some(artists) = artist_cell(t, album_artist) {
                    text(3, artists, color, false);
                }
            }
            _ => {}
        }
    }
    if row.status == EntryStatus::Missing {
        let path = row.raw_path.unwrap_or_default().to_string();
        response.on_hover_text(tr!(ui, "asteria-music-tracks-missing-hover", path = path));
        return None;
    }
    if row.status == EntryStatus::Moved && response.hovered() {
        response
            .clone()
            .on_hover_text(tr!(ui, "asteria-music-tracks-moved-hover"));
    }
    if !can_play {
        return None;
    }
    let mut action = response.clicked().then_some(TrackAction::Play(index));
    response.context_menu(|ui| {
        if ui
            .button(tr!(ui, "asteria-music-tracks-play-next"))
            .clicked()
        {
            action = Some(TrackAction::PlayNext(index));
            ui.close();
        }
        if ui.button(tr!(ui, "asteria-music-tracks-append")).clicked() {
            action = Some(TrackAction::Append(index));
            ui.close();
        }
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use asteria_music_api::{ItemId, MediaItem, MediaKind, TrackTags};

    fn track(artists: &[&str], album_artist: Option<&str>) -> MusicTrack {
        MusicTrack {
            item: MediaItem {
                id: ItemId("t".into()),
                path: "t.flac".into(),
                kind: MediaKind::Audio,
                size: 0,
                modified: "2026-01-01T00:00:00Z".into(),
                rating: None,
                subjects: vec![],
                hierarchical_subjects: vec![],
                description: None,
                label: None,
                content_hash: None,
                file_hash: None,
                read_only_reason: None,
            },
            tags: TrackTags {
                artists: artists.iter().map(|s| s.to_string()).collect(),
                album_artist: album_artist.map(str::to_string),
                ..Default::default()
            },
            display_title: "t".into(),
        }
    }

    #[test]
    fn durations_read_as_minutes_and_seconds() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(61_999), "1:01");
        assert_eq!(format_duration(3_600_000), "60:00");
    }

    #[test]
    fn the_artist_shows_only_when_it_differs() {
        assert_eq!(artist_cell(&track(&["A"], None), Some("A")), None);
        assert_eq!(artist_cell(&track(&["A"], Some("A")), None), None);
        assert_eq!(
            artist_cell(&track(&["A", "B"], None), Some("A")),
            Some("A, B".into())
        );
        assert_eq!(artist_cell(&track(&[], None), Some("A")), None);
    }
}
