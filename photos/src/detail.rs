//! The detail pane: the selected item's preview, its file's facts and its metadata, and the
//! edits to them, sent as `media.update`.
//!
//! What the edits wait for ([`Edits`]: the requests on their way, the one resend after
//! `BUSY`, the last error, the text being typed) is held by the [`BrowseState`] of the
//! server in use, so it is dropped with that server's [`Session`] on a switch; an answer
//! that comes after is never applied (the session drops it, and nothing here waits for it
//! any more).
//!
//! The pane shows the server's values only: an edit changes nothing on screen until the
//! server returns the updated [`MediaItem`], which then replaces the item in the listing.
//! While an edit is on its way, its editor is disabled. If it fails, the editor is usable
//! again and still shows the server's value.
//!
//! - **Rating**: clicking a star sets that many stars; clicking the star of the rating
//!   already set clears it to 0 ([`rating_after_click`]).
//! - **Description**: saved when the field loses focus, or when another item is selected
//!   (or the folder changes) with text typed but not yet saved. There is no Save button.
//! - **Read-only items** (`read_only_reason`, or a `READ_ONLY` answer): the reason shows in
//!   place of the editors.
//! - **`BUSY`**: sent once more after [`BUSY_RETRY_DELAY`] (scheduled, never slept on the UI
//!   thread); a second `BUSY` shows an error.
//!
//! [`BrowseState`]: crate::browse::BrowseState

use std::time::{Duration, Instant};

use asteria_framework_api::{ItemId, MediaItem, MediaKind, MediaUpdateParams, codes};
use asteria_framework_client::errors::{error_detail, error_text};
use asteria_framework_client::images::{ImageState, Preset};
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::{Reply, Request, RequestId, Response, Session};
use corundum_framework::egui;
use corundum_framework::gui::theme::{self, Swatch, Tone};
use corundum_framework::gui::{icons, tr};

/// How long an edit the server answered `BUSY` waits before it is sent again (once).
pub const BUSY_RETRY_DELAY: Duration = Duration::from_millis(500);
/// The most stars.
pub const MAX_RATING: i8 = 5;
/// The colour labels offered, as written to `xmp:Label` (Lightroom's names).
pub const LABELS: [&str; 5] = ["Red", "Yellow", "Green", "Blue", "Purple"];
/// The preview's greatest height in the pane.
const PREVIEW_HEIGHT: f32 = 280.0;

/// A metadata field the pane edits. Each is sent, and disabled while sending, on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Rating,
    Label,
    /// `dc:subject`.
    Subjects,
    /// `lr:hierarchicalSubject`.
    HierarchicalSubjects,
    Description,
}

/// One change to an item's metadata: the field's new value, whole.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// 0 (none) to [`MAX_RATING`].
    Rating(i8),
    /// `None` removes the label.
    Label(Option<String>),
    Subjects(Vec<String>),
    HierarchicalSubjects(Vec<String>),
    /// `None` removes the description.
    Description(Option<String>),
}

impl Edit {
    pub fn field(&self) -> Field {
        match self {
            Edit::Rating(_) => Field::Rating,
            Edit::Label(_) => Field::Label,
            Edit::Subjects(_) => Field::Subjects,
            Edit::HierarchicalSubjects(_) => Field::HierarchicalSubjects,
            Edit::Description(_) => Field::Description,
        }
    }

    fn params(&self, id: &ItemId) -> MediaUpdateParams {
        let mut p = MediaUpdateParams {
            id: id.clone(),
            rating: None,
            subjects: None,
            hierarchical_subjects: None,
            description: None,
            label: None,
        };
        match self.clone() {
            Edit::Rating(r) => p.rating = Some(r),
            Edit::Label(l) => p.label = Some(l),
            Edit::Subjects(s) => p.subjects = Some(s),
            Edit::HierarchicalSubjects(s) => p.hierarchical_subjects = Some(s),
            Edit::Description(d) => p.description = Some(d),
        }
        p
    }
}

/// The rating a click on star `star` (1 to 5) makes: that many stars, or none when it is
/// the rating already set.
pub fn rating_after_click(current: Option<i8>, star: i8) -> i8 {
    if current == Some(star) { 0 } else { star }
}

/// Why the last edit of an item was not saved.
#[derive(Clone, Debug, PartialEq)]
pub enum EditError {
    /// The server answered `BUSY` twice.
    Busy,
    /// Any other failure (a `READ_ONLY` answer makes the item read-only instead).
    Failed(ClientError),
}

/// An edit on its way, or waiting to be sent again after `BUSY`.
#[derive(Debug)]
struct Sending {
    item: ItemId,
    edit: Edit,
    /// The request waited for; `None` while waiting for `retry_at`.
    request: Option<RequestId>,
    retry_at: Option<Instant>,
    /// Whether this is already the resend.
    retried: bool,
}

/// The text typed in the pane for one item, not yet sent.
#[derive(Debug)]
struct Inputs {
    item: ItemId,
    description: String,
    /// The server's description when `description` was last set from it.
    description_base: String,
    subject: String,
    hierarchical: String,
}

impl Inputs {
    fn new(item: &MediaItem) -> Inputs {
        let description = item.description.clone().unwrap_or_default();
        Inputs {
            item: item.id.clone(),
            description_base: description.clone(),
            description,
            subject: String::new(),
            hierarchical: String::new(),
        }
    }

    fn description_changed(&self) -> bool {
        self.description != self.description_base
    }
}

/// The edits of one server's items: kept in its [`BrowseState`](crate::browse::BrowseState).
#[derive(Debug, Default)]
pub struct Edits {
    sending: Vec<Sending>,
    error: Option<(ItemId, EditError)>,
    inputs: Option<Inputs>,
}

impl Edits {
    /// Send `edit` of `item`, unless the item is read-only or that field of it is still on
    /// its way. Returns whether it was sent.
    pub(crate) fn send(&mut self, session: &mut Session, item: &MediaItem, edit: Edit) -> bool {
        if item.read_only_reason.is_some() || self.is_sending(&item.id, edit.field()) {
            return false;
        }
        self.send_to(session, item.id.clone(), edit);
        true
    }

    fn send_to(&mut self, session: &mut Session, item: ItemId, edit: Edit) {
        if self.error.as_ref().is_some_and(|(i, _)| *i == item) {
            self.error = None;
        }
        let request = session.request(Request::MediaUpdate(edit.params(&item)));
        self.sending.push(Sending {
            item,
            edit,
            request: Some(request),
            retry_at: None,
            retried: false,
        });
    }

    /// Whether an edit of `field` of `item` is on its way (or waiting to be sent again).
    pub fn is_sending(&self, item: &ItemId, field: Field) -> bool {
        self.sending
            .iter()
            .any(|s| s.item == *item && s.edit.field() == field)
    }

    /// Why the last edit of `item` failed, until the next edit of it.
    pub fn error(&self, item: &ItemId) -> Option<&EditError> {
        self.error
            .as_ref()
            .filter(|(i, _)| i == item)
            .map(|(_, e)| e)
    }

    /// Take `reply` if it answers an edit; the returned item then replaces its namesake in
    /// `items`. Returns whether the reply was an edit's.
    pub(crate) fn handle(&mut self, reply: &Reply, items: &mut [MediaItem]) -> bool {
        let Some(pos) = self
            .sending
            .iter()
            .position(|s| s.request == Some(reply.id))
        else {
            return false;
        };
        let s = &mut self.sending[pos];
        match &reply.result {
            Err(ClientError::Rpc { code, .. }) if *code == codes::BUSY && !s.retried => {
                // Sent again by `tick` once the delay is over; the editor stays disabled.
                s.request = None;
                s.retry_at = Some(Instant::now() + BUSY_RETRY_DELAY);
                s.retried = true;
                return true;
            }
            _ => {}
        }
        let s = self.sending.remove(pos);
        match &reply.result {
            Ok(Response::Media(updated)) => {
                if let Some(slot) = items.iter_mut().find(|i| i.id == updated.id) {
                    *slot = updated.clone();
                }
            }
            Err(ClientError::Rpc { code, message }) if *code == codes::READ_ONLY => {
                // The same view as an item listed read-only.
                if let Some(slot) = items.iter_mut().find(|i| i.id == s.item)
                    && slot.read_only_reason.is_none()
                {
                    slot.read_only_reason = Some(message.clone());
                }
            }
            Err(ClientError::Rpc { code, .. }) if *code == codes::BUSY => {
                self.error = Some((s.item.clone(), EditError::Busy));
            }
            Err(e) => self.error = Some((s.item.clone(), EditError::Failed(e.clone()))),
            Ok(other) => {
                let e = ClientError::Protocol(format!("media.update answered with {other:?}"));
                self.error = Some((s.item.clone(), EditError::Failed(e)));
            }
        }
        // The description field shows the server's text again, whatever the outcome.
        if s.edit.field() == Field::Description
            && let Some(inputs) = &mut self.inputs
            && inputs.item == s.item
            && let Some(item) = items.iter().find(|i| i.id == s.item)
        {
            let text = item.description.clone().unwrap_or_default();
            inputs.description_base = text.clone();
            inputs.description = text;
        }
        true
    }

    /// Per-frame upkeep: save a description typed for an item no longer `selected`, and
    /// send again the edits whose `BUSY` delay is over (waking egui for the next one).
    pub(crate) fn tick(
        &mut self,
        session: &mut Session,
        ctx: &egui::Context,
        selected: Option<&ItemId>,
        items: &[MediaItem],
    ) {
        if self
            .inputs
            .as_ref()
            .is_some_and(|i| Some(&i.item) != selected)
        {
            let inputs = self.inputs.take().expect("checked above");
            if inputs.description_changed() {
                let edit = Edit::Description(non_empty(inputs.description));
                match items.iter().find(|i| i.id == inputs.item) {
                    Some(item) => {
                        self.send(session, item, edit);
                    }
                    // The folder was left: the item is not listed, but the text is not lost.
                    None => self.send_to(session, inputs.item, edit),
                }
            }
        }
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        for s in &mut self.sending {
            let Some(at) = s.retry_at else { continue };
            if at <= now {
                s.retry_at = None;
                s.request = Some(session.request(Request::MediaUpdate(s.edit.params(&s.item))));
            } else {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }
        if let Some(at) = next {
            ctx.request_repaint_after(at - now);
        }
    }
}

/// What the pane shows for one item, as the tests (and the pane) read it.
#[derive(Debug)]
pub struct DetailView<'a> {
    item: &'a MediaItem,
    edits: &'a Edits,
}

impl<'a> DetailView<'a> {
    pub(crate) fn new(item: &'a MediaItem, edits: &'a Edits) -> DetailView<'a> {
        DetailView { item, edits }
    }

    /// The item, as the server last returned it.
    pub fn item(&self) -> &'a MediaItem {
        self.item
    }

    /// Why the item cannot be edited (shown in place of the editors).
    pub fn read_only_reason(&self) -> Option<&'a str> {
        self.item.read_only_reason.as_deref()
    }

    /// Whether the editors are shown (else the values are shown as text).
    pub fn editors_shown(&self) -> bool {
        self.read_only_reason().is_none()
    }

    /// Whether `field`'s editor is disabled, its edit on its way.
    pub fn is_sending(&self, field: Field) -> bool {
        self.edits.is_sending(&self.item.id, field)
    }

    /// Why the last edit failed.
    pub fn error(&self) -> Option<&'a EditError> {
        self.edits.error(&self.item.id)
    }
}

/// Draw the pane for `item` into `ui` (the side pane's column, card or screen). Returns
/// whether "View full screen" was clicked.
pub(crate) fn ui(
    ui: &mut egui::Ui,
    item: &MediaItem,
    edits: &mut Edits,
    session: &mut Session,
) -> bool {
    let mut open = false;
    preview_ui(ui, item, session);
    if item.kind == MediaKind::Video {
        ui.weak(tr!(ui, "asteria-photos-detail-video"));
    }
    ui.add_space(4.0);
    let (folder, name) = match item.path.rsplit_once('/') {
        Some((folder, name)) => (Some(folder), name),
        None => (None, item.path.as_str()),
    };
    ui.heading(name);
    if ui.button(tr!(ui, "asteria-photos-detail-open")).clicked() {
        open = true;
    }
    ui.add_space(4.0);
    egui::Grid::new(("asteria_photos_detail_facts", &item.id.0))
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            if let Some(folder) = folder {
                ui.weak(tr!(ui, "asteria-photos-detail-folder"));
                ui.label(folder);
                ui.end_row();
            }
            ui.weak(tr!(ui, "asteria-photos-detail-size"));
            ui.label(format_size(item.size));
            ui.end_row();
            ui.weak(tr!(ui, "asteria-photos-detail-modified"));
            ui.label(format_modified(&item.modified));
            ui.end_row();
        });
    ui.separator();

    let view = DetailView::new(item, edits);
    if let Some(reason) = view.read_only_reason() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            tr!(ui, "asteria-photos-detail-read-only"),
        );
        ui.weak(tr!(
            ui,
            "asteria-photos-detail-read-only-reason",
            reason = reason
        ));
        ui.add_space(4.0);
        values_ui(ui, item);
        return open;
    }
    if let Some(e) = view.error() {
        let (text, detail) = match e {
            EditError::Busy => (tr!(ui, "asteria-photos-detail-busy"), None),
            EditError::Failed(e) => {
                let reason = error_text(ui.ctx(), e);
                (
                    tr!(ui, "asteria-photos-detail-save-failed", reason = reason),
                    error_detail(e),
                )
            }
        };
        let label = ui.colored_label(ui.visuals().error_fg_color, text);
        if let Some(detail) = detail {
            label.on_hover_text(detail);
        }
    }
    if edits.sending.iter().any(|s| s.item == item.id) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(tr!(ui, "asteria-photos-detail-saving"));
        });
    }
    editors_ui(ui, item, edits, session);
    open
}

/// The preview, as large as fits the pane's width.
fn preview_ui(ui: &mut egui::Ui, item: &MediaItem, session: &mut Session) {
    let max = egui::vec2(ui.available_width(), PREVIEW_HEIGHT);
    match session.images().request(item, Preset::Preview) {
        ImageState::Ready(tex) => {
            let size = fit(tex.size_vec2(), max);
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                tex.id(),
                size,
            )));
        }
        ImageState::Loading => {
            ui.allocate_ui(egui::vec2(max.x, max.y / 2.0), |ui| {
                ui.centered_and_justified(|ui| ui.spinner());
            });
        }
        ImageState::Failed(e) => {
            let reason = error_text(ui.ctx(), &e);
            ui.weak(tr!(
                ui,
                "asteria-photos-detail-preview-failed",
                reason = reason
            ));
        }
    }
}

/// The metadata as text, for a read-only item.
fn values_ui(ui: &mut egui::Ui, item: &MediaItem) {
    egui::Grid::new(("asteria_photos_detail_values", &item.id.0))
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            ui.weak(tr!(ui, "asteria-photos-detail-rating"));
            match item.rating.filter(|r| *r > 0) {
                Some(r) => {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for _ in 0..r {
                            let star = icons::star();
                            ui.add(icons::image_as(ui, star, 14.0, Tone::Text, Swatch::Yellow));
                        }
                    });
                }
                None => {
                    ui.label(tr!(ui, "asteria-photos-detail-rating-none"));
                }
            }
            ui.end_row();
            ui.weak(tr!(ui, "asteria-photos-detail-label"));
            ui.label(label_name(ui, item.label.as_deref()));
            ui.end_row();
            ui.weak(tr!(ui, "asteria-photos-detail-subjects"));
            ui.label(tags_text(ui, &item.subjects));
            ui.end_row();
            ui.weak(tr!(ui, "asteria-photos-detail-hierarchical"));
            ui.label(tags_text(ui, &item.hierarchical_subjects));
            ui.end_row();
        });
    ui.weak(tr!(ui, "asteria-photos-detail-description"));
    match item.description.as_deref().filter(|d| !d.is_empty()) {
        Some(d) => ui.label(d),
        None => ui.weak(tr!(ui, "asteria-photos-detail-description-empty")),
    };
}

fn tags_text(ui: &egui::Ui, tags: &[String]) -> String {
    if tags.is_empty() {
        tr!(ui, "asteria-photos-detail-tags-empty")
    } else {
        tags.join(", ")
    }
}

/// The editors of an item that can be edited.
fn editors_ui(ui: &mut egui::Ui, item: &MediaItem, edits: &mut Edits, session: &mut Session) {
    if edits.inputs.as_ref().is_none_or(|i| i.item != item.id) {
        edits.inputs = Some(Inputs::new(item));
    }
    let mut edit: Option<Edit> = None;
    let [
        rating_busy,
        label_busy,
        subjects_busy,
        hierarchical_busy,
        description_busy,
    ] = [
        Field::Rating,
        Field::Label,
        Field::Subjects,
        Field::HierarchicalSubjects,
        Field::Description,
    ]
    .map(|f| edits.is_sending(&item.id, f));

    ui.weak(tr!(ui, "asteria-photos-detail-rating"));
    ui.add_enabled_ui(!rating_busy, |ui| {
        if let Some(r) = stars_ui(ui, item.rating) {
            edit = Some(Edit::Rating(r));
        }
    });
    ui.add_space(4.0);

    ui.weak(tr!(ui, "asteria-photos-detail-label"));
    ui.add_enabled_ui(!label_busy, |ui| {
        if let Some(l) = label_ui(ui, item) {
            edit = Some(Edit::Label(l));
        }
    });
    ui.add_space(4.0);

    let inputs = edits.inputs.as_mut().expect("set above");
    ui.weak(tr!(ui, "asteria-photos-detail-subjects"));
    ui.add_enabled_ui(!subjects_busy, |ui| {
        let hint = tr!(ui, "asteria-photos-detail-subjects-add");
        if let Some(tags) = tags_ui(ui, "subjects", &item.subjects, &mut inputs.subject, hint) {
            edit = Some(Edit::Subjects(tags));
        }
    });
    ui.add_space(4.0);

    ui.weak(tr!(ui, "asteria-photos-detail-hierarchical"));
    ui.add_enabled_ui(!hierarchical_busy, |ui| {
        let hint = tr!(ui, "asteria-photos-detail-hierarchical-add");
        let current = &item.hierarchical_subjects;
        if let Some(tags) = tags_ui(ui, "hierarchical", current, &mut inputs.hierarchical, hint) {
            edit = Some(Edit::HierarchicalSubjects(tags));
        }
    });
    ui.add_space(4.0);

    ui.weak(tr!(ui, "asteria-photos-detail-description"));
    // The server's text changed (another client, say) while nothing was typed: show it.
    let server_text = item.description.clone().unwrap_or_default();
    if !inputs.description_changed() && inputs.description_base != server_text {
        inputs.description_base = server_text.clone();
        inputs.description = server_text;
    }
    let response = ui.add_enabled(
        !description_busy,
        egui::TextEdit::multiline(&mut inputs.description)
            .id_salt(("asteria_photos_description", &item.id.0))
            .hint_text(tr!(ui, "asteria-photos-detail-description-hint"))
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    if response.lost_focus() && inputs.description_changed() {
        edit = Some(Edit::Description(non_empty(inputs.description.clone())));
    }

    if let Some(edit) = edit {
        edits.send(session, item, edit);
    }
}

/// Five stars; returns the rating a click asks for.
fn stars_ui(ui: &mut egui::Ui, rating: Option<i8>) -> Option<i8> {
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
            let image = icons::image_as(ui, icons::star(), 18.0, tone, swatch);
            let hover = if rating == Some(star) {
                tr!(ui, "asteria-photos-detail-rating-clear")
            } else {
                tr!(
                    ui,
                    "asteria-photos-detail-rating-set",
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

/// The colour label menu, with the current label's colour beside it; returns the label
/// chosen (`Some(None)` for none).
fn label_ui(ui: &mut egui::Ui, item: &MediaItem) -> Option<Option<String>> {
    let current = item.label.as_deref();
    let mut chosen = None;
    ui.horizontal(|ui| {
        label_dot(ui, current);
        egui::ComboBox::from_id_salt(("asteria_photos_label", &item.id.0))
            .selected_text(label_name(ui, current))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(current.is_none(), label_name(ui, None))
                    .clicked()
                    && current.is_some()
                {
                    chosen = Some(None);
                }
                // A label of another name (written elsewhere) stays offered as it is.
                let other = current.filter(|c| !LABELS.contains(c));
                for name in LABELS.iter().copied().chain(other) {
                    let here = current == Some(name);
                    if ui
                        .selectable_label(here, label_name(ui, Some(name)))
                        .clicked()
                        && !here
                    {
                        chosen = Some(Some(name.to_string()));
                    }
                }
            });
    });
    chosen
}

/// A small disc in the label's colour (none for no label or one of another name).
fn label_dot(ui: &mut egui::Ui, label: Option<&str>) {
    let swatch = match label {
        Some("Red") => Swatch::Red,
        Some("Yellow") => Swatch::Yellow,
        Some("Green") => Swatch::Green,
        Some("Blue") => Swatch::Blue,
        Some("Purple") => Swatch::Purple,
        _ => return,
    };
    let colour = theme::current(ui.ctx()).color(Tone::Text, swatch);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 6.0, colour);
}

/// The label's name in the user's language; a label of another name as it is.
fn label_name(ui: &egui::Ui, label: Option<&str>) -> String {
    match label {
        None => tr!(ui, "asteria-photos-detail-label-none"),
        Some("Red") => tr!(ui, "asteria-photos-detail-label-red"),
        Some("Yellow") => tr!(ui, "asteria-photos-detail-label-yellow"),
        Some("Green") => tr!(ui, "asteria-photos-detail-label-green"),
        Some("Blue") => tr!(ui, "asteria-photos-detail-label-blue"),
        Some("Purple") => tr!(ui, "asteria-photos-detail-label-purple"),
        Some(other) => other.to_string(),
    }
}

/// The tags, each a button that removes it, and a field adding one (Enter or "Add").
/// Returns the whole new list when one was added or removed.
fn tags_ui(
    ui: &mut egui::Ui,
    salt: &str,
    tags: &[String],
    input: &mut String,
    hint: String,
) -> Option<Vec<String>> {
    let mut changed = None;
    if tags.is_empty() {
        ui.weak(tr!(ui, "asteria-photos-detail-tags-empty"));
    } else {
        ui.horizontal_wrapped(|ui| {
            for (i, tag) in tags.iter().enumerate() {
                let x = icons::image(ui, icons::x(), 10.0);
                if ui
                    .add(egui::Button::image_and_text(x, tag.as_str()))
                    .on_hover_text(tr!(
                        ui,
                        "asteria-photos-detail-tag-remove",
                        tag = tag.as_str()
                    ))
                    .clicked()
                {
                    let mut rest = tags.to_vec();
                    rest.remove(i);
                    changed = Some(rest);
                }
            }
        });
    }
    ui.horizontal(|ui| {
        let add_label = tr!(ui, "asteria-photos-detail-tag-add");
        let button_width = 60.0;
        let field = ui.add(
            egui::TextEdit::singleline(input)
                .id_salt(("asteria_photos_tag_input", salt))
                .hint_text(hint)
                .desired_width((ui.available_width() - button_width).max(80.0)),
        );
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if entered {
            // The field gave up focus but left the key: the grid, drawn next, would take
            // it as "open the selection".
            ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        }
        let clicked = ui.button(add_label).clicked();
        if entered || clicked {
            let tag = input.trim().to_string();
            input.clear();
            if !tag.is_empty() && !tags.contains(&tag) {
                let mut all = tags.to_vec();
                all.push(tag);
                changed = Some(all);
            }
        }
    });
    changed
}

/// `text`, or `None` when it is empty.
fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

/// `src` scaled to fit within `max`, keeping its proportions.
pub(crate) fn fit(src: egui::Vec2, max: egui::Vec2) -> egui::Vec2 {
    if src.x <= 0.0 || src.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (max.x / src.x).min(max.y / src.y).max(0.0);
    src * scale
}

/// A byte count in B, KB, MB or GB (powers of 1000), one decimal above bytes.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// An RFC 3339 UTC time as `YYYY-MM-DD HH:MM UTC`; anything else as it is.
pub fn format_modified(rfc3339: &str) -> String {
    let b = rfc3339.as_bytes();
    let shaped = b.len() >= 16
        && b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':';
    // `get` rather than indexing: a multi-byte character can make a byte offset fall inside
    // it, and server data must not panic the UI.
    match (rfc3339.get(..10), rfc3339.get(11..16)) {
        (Some(date), Some(time))
            if shaped && (rfc3339.ends_with('Z') || rfc3339.ends_with("+00:00")) =>
        {
            format!("{date} {time} UTC")
        }
        _ => rfc3339.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_decimal_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1000), "1.0 KB");
        assert_eq!(format_size(1_234_567), "1.2 MB");
        assert_eq!(format_size(999_999), "1.0 MB");
        assert_eq!(format_size(3_000_000_000), "3.0 GB");
    }

    #[test]
    fn times_drop_seconds_and_say_utc() {
        assert_eq!(
            format_modified("2026-01-02T03:04:05Z"),
            "2026-01-02 03:04 UTC"
        );
        assert_eq!(
            format_modified("2026-01-02T03:04:05.123+00:00"),
            "2026-01-02 03:04 UTC"
        );
        assert_eq!(format_modified("yesterday"), "yesterday");
        // A byte offset inside a character: shown as it is, never a panic.
        assert_eq!(
            format_modified("2026-01-02T03:0\u{e9}Z"),
            "2026-01-02T03:0\u{e9}Z"
        );
    }

    #[test]
    fn each_edit_sends_its_field_only() {
        let id = ItemId("x".into());
        let p = Edit::Rating(3).params(&id);
        assert_eq!((p.rating, p.label.clone()), (Some(3), None));
        let p = Edit::Label(None).params(&id);
        assert_eq!((p.rating, p.label), (None, Some(None)));
        let p = Edit::Description(Some("d".into())).params(&id);
        assert_eq!(p.description, Some(Some("d".into())));
        assert_eq!(p.subjects, None);
    }

    #[test]
    fn fitting_keeps_proportions() {
        let s = fit(egui::vec2(200.0, 100.0), egui::vec2(100.0, 100.0));
        assert_eq!(s, egui::vec2(100.0, 50.0));
        let s = fit(egui::vec2(100.0, 400.0), egui::vec2(300.0, 200.0));
        assert_eq!(s, egui::vec2(50.0, 200.0));
    }
}
