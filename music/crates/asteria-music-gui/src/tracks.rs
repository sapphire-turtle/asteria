//! The table of an album's or a playlist's tracks, drawn row by row (only the rows on screen).
//!
//! Columns: disc, number, title, the artist (only when it differs from the album artist), the
//! rating (wide only) and the length as `m:ss`. In a playlist a [`Missing`](EntryStatus::Missing)
//! entry is dimmed, with the path written in the playlist, and a [`Moved`](EntryStatus::Moved)
//! one is marked. Offline, a track whose audio is not saved is dimmed too ([`Availability`]). A
//! click on a row plays from it; the right-click menu has "play next" and "add to queue".
//!
//! Wide, each track's rating is five stars in its row; a click on one rates the track and does
//! not play it. Narrow there is no room for them, so the right-click menu has a "Rating"
//! submenu instead. Rating needs no audio, so offline every track can be rated, saved or not;
//! a missing playlist entry cannot. The table only reports what the user asked for
//! ([`TrackAction`]); the app turns it into a command, or a rating.

use asteria_common_client::offline::OfflineHandle;
use asteria_common_client::session::Session;
use asteria_common_client::widgets::stars::{MAX_RATING, stars};
use asteria_music_api::{Album, EntryStatus, MediaItem, MusicTrack, Playlist};
use corundum_framework::egui::{self, Align, Align2, Layout, Sense, vec2};
use corundum_framework::gui::tr;

use crate::player::Quality;

/// What the user asked of a row. The index is the row's: an album's track, a playlist's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackAction {
    Play(usize),
    PlayNext(usize),
    Append(usize),
    /// Rate the row's track: 0 (none) to [`MAX_RATING`].
    Rate(usize, i8),
}

/// Which tracks can be played now: every one while the server answers; offline, only those
/// whose offline audio is saved. Asked of the handle's record of what is stored, so a frame
/// that draws many rows does no disk I/O.
#[derive(Clone, Debug, Default)]
pub enum Availability {
    #[default]
    All,
    /// Offline, with the library's copies (`None` when the app keeps none).
    Saved(Option<OfflineHandle>),
}

impl Availability {
    pub fn of(session: &Session) -> Availability {
        if session.is_offline() {
            Availability::Saved(session.offline().cloned())
        } else {
            Availability::All
        }
    }

    pub fn can_play(&self, track: &MusicTrack) -> bool {
        match self {
            Availability::All => true,
            Availability::Saved(handle) => handle
                .as_ref()
                .zip(track.item.file_hash.as_deref())
                .is_some_and(|(h, hash)| h.is_stored(hash, Quality::Low.preset())),
        }
    }
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
/// A row's star, and the room around it (each side) and between two.
const STAR: f32 = 12.0;
const STAR_PAD: f32 = 1.0;
const STAR_GAP: f32 = 2.0;
/// The rating column: five stars, then a gap before the length.
const RATING_W: f32 =
    MAX_RATING as f32 * (STAR + 2.0 * STAR_PAD) + (MAX_RATING as f32 - 1.0) * STAR_GAP + PAD;

/// What the table needs besides its rows.
struct Context<'a> {
    album_artist: Option<&'a str>,
    available: &'a Availability,
    ratings: &'a dyn Fn(&MediaItem) -> Option<i8>,
    wide: bool,
    font: egui::FontId,
    row_h: f32,
}

/// Draws the table. `album_artist` is the album's artist, if the rows all belong to one album;
/// `available` says which tracks can be played and `ratings` the rating a track shows (both
/// asked only for the rows drawn). `wide` puts the stars in the rows; narrow, they are in the
/// right-click menu. Returns what the user asked for this frame.
pub fn show(
    ui: &mut egui::Ui,
    salt: impl egui::AsIdSalt,
    rows: &[Row<'_>],
    album_artist: Option<&str>,
    available: &Availability,
    ratings: &dyn Fn(&MediaItem) -> Option<i8>,
    wide: bool,
) -> Option<TrackAction> {
    let cx = Context {
        album_artist,
        available,
        ratings,
        wide,
        font: egui::TextStyle::Body.resolve(ui.style()),
        row_h: ui.text_style_height(&egui::TextStyle::Body) + 8.0,
    };
    let mut action = None;
    header(ui, &cx);
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .auto_shrink([false, false])
        .show_rows(ui, cx.row_h, rows.len(), |ui, range| {
            for index in range {
                if let Some(a) = row_ui(ui, index, &rows[index], &cx) {
                    action = Some(a);
                }
            }
        });
    action
}

/// The column rectangles of a row `rect` wide: disc, number, title, artist, rating, length.
/// Narrow (`wide` false) the rating's is empty.
fn columns(rect: egui::Rect, wide: bool) -> [egui::Rect; 6] {
    let rect = rect.shrink2(vec2(PAD, 0.0));
    let rating_w = if wide { RATING_W } else { 0.0 };
    let flexible = (rect.width() - DISC_W - NUMBER_W - rating_w - LENGTH_W).max(0.0);
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
        next(rating_w),
        next(LENGTH_W),
    ]
}

/// The label of `rating` in the narrow rows' "Rating" submenu: a star for each, or "No rating".
fn rating_label(ui: &egui::Ui, rating: i8) -> String {
    if rating <= 0 {
        tr!(ui, "asteria-music-tracks-rate-none")
    } else {
        "★".repeat(rating as usize)
    }
}

fn header(ui: &mut egui::Ui, cx: &Context<'_>) {
    let font = &cx.font;
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), font.size + 8.0), Sense::hover());
    let cols = columns(rect, cx.wide);
    let color = ui.visuals().weak_text_color();
    let painter = ui.painter_at(rect);
    let keys = [
        "asteria-music-tracks-disc",
        "asteria-music-tracks-number",
        "asteria-music-tracks-title",
        "asteria-music-tracks-artist",
        "asteria-music-tracks-rate",
        "asteria-music-tracks-length",
    ];
    for (i, (col, key)) in cols.iter().zip(keys).enumerate() {
        if col.width() <= 0.0 {
            continue;
        }
        let text = tr!(ui, key);
        let (align, x) = if i == 5 {
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

fn row_ui(ui: &mut egui::Ui, index: usize, row: &Row<'_>, cx: &Context<'_>) -> Option<TrackAction> {
    let (font, row_h, album_artist) = (&cx.font, cx.row_h, cx.album_artist);
    let saved = row.track.is_none_or(|t| cx.available.can_play(t));
    // Rating needs no audio: every track found can be rated, saved or not.
    let rateable = playable(row).then_some(row.track).flatten();
    let can_play = rateable.is_some() && saved;
    // Narrow, a track that cannot be played still has the menu, for its rating.
    let sense = if can_play || (rateable.is_some() && !cx.wide) {
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
        let color = if row.status == EntryStatus::Missing || !saved {
            weak
        } else {
            strong
        };
        let cols = columns(rect, cx.wide);
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
                text(5, format_duration(ms), weak, true);
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
    let Some(track) = rateable else {
        if row.status == EntryStatus::Missing {
            let path = row.raw_path.unwrap_or_default().to_string();
            response.on_hover_text(tr!(ui, "asteria-music-tracks-missing-hover", path = path));
        }
        return None;
    };
    let rating = (cx.ratings)(&track.item);
    let mut action = None;
    if cx.wide {
        // Registered after the row, so a click on a star is the star's, not the row's; and a
        // click that just misses one (between or around the stars) is the column's, which
        // does nothing, so it never plays.
        let col = columns(rect, true)[4];
        ui.interact(
            col,
            ui.scope_id().with(("asteria_music_track_rating", index)),
            Sense::click(),
        );
        let mut stars_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("asteria_music_track_stars", index))
                .max_rect(col)
                .layout(Layout::left_to_right(Align::Center)),
        );
        stars_ui.spacing_mut().button_padding = vec2(STAR_PAD, STAR_PAD);
        if let Some(r) = stars(&mut stars_ui, rating, STAR) {
            action = Some(TrackAction::Rate(index, r));
        }
    }
    if can_play && response.clicked() {
        action = Some(TrackAction::Play(index));
    }
    // Wide, the menu has only what plays; a track that cannot be played has none.
    if can_play || !cx.wide {
        response.context_menu(|ui| {
            if can_play {
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
            }
            if !cx.wide {
                ui.menu_button(tr!(ui, "asteria-music-tracks-rate"), |ui| {
                    let set = rating.unwrap_or(0);
                    for r in 0..=MAX_RATING {
                        if ui.selectable_label(set == r, rating_label(ui, r)).clicked() {
                            action = Some(TrackAction::Rate(index, r));
                            ui.close();
                        }
                    }
                });
            }
        });
    }
    if !saved {
        response.on_hover_text(tr!(ui, "asteria-music-offline-not-saved"));
    } else if row.status == EntryStatus::Moved && response.hovered() {
        response.on_hover_text(tr!(ui, "asteria-music-tracks-moved-hover"));
    }
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
    fn online_every_track_plays_and_offline_without_copies_none() {
        let t = track(&["A"], None);
        assert!(Availability::All.can_play(&t));
        assert!(!Availability::Saved(None).can_play(&t));
    }

    #[test]
    fn wide_rows_rate_between_the_artist_and_the_length_and_narrow_rows_do_not() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 30.0));
        let wide = columns(rect, true);
        assert_eq!(wide[4].width(), RATING_W);
        assert_eq!(wide[3].right(), wide[4].left());
        assert_eq!(wide[4].right(), wide[5].left());
        // Five stars with their padding and gaps fit, with a gap left before the length.
        let stars = 5.0 * (STAR + 2.0 * STAR_PAD) + 4.0 * STAR_GAP;
        assert!(stars < RATING_W, "{stars} in {RATING_W}");
        let narrow = columns(rect, false);
        assert_eq!(narrow[4].width(), 0.0);
        assert_eq!(narrow[5].right(), wide[5].right());
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
