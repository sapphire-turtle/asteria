//! The library screen: where in the library the user is, what that folder holds, and the
//! requests that fetch it.
//!
//! A [`BrowseState`] belongs to one server: the app keeps it beside that server's
//! [`Session`] and drops both together on a switch, so a listing can never outlive its
//! server. Replies are matched by the [`RequestId`] the state is waiting for; any other reply
//! (an older page, a folder left since) is ignored.
//!
//! It also holds what the detail pane's edits wait for ([`Edits`]) and the item shown full
//! screen ([`opened`](BrowseState::opened)), for the same reason.

use std::time::{Duration, Instant};

use asteria_framework_api::{FolderListParams, ItemId, MediaItem};
use asteria_framework_client::errors::{error_detail, error_text};
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::{
    Reply, Request, RequestId, Response, Session, SessionState,
};
use asteria_framework_client::widgets::thumb_grid::ThumbGrid;
use corundum_framework::egui;
use corundum_framework::gui::{icons, tr};

use crate::detail::{DetailView, Edit, Edits};

/// Items asked for per `folder.list` page.
pub const PAGE_SIZE: u32 = 200;
/// How often `library.info` is asked again while the server scans.
pub const SCANNING_POLL: Duration = Duration::from_secs(2);
/// How often it is asked otherwise, to notice changes the server's watcher found.
pub const IDLE_POLL: Duration = Duration::from_secs(30);
/// The next page is asked for once the drawn rows come within this many rows of the end.
const PREFETCH_ROWS: usize = 3;
/// The viewer asks for the next page once it comes within this many items of the end.
const PREFETCH_ITEMS: usize = 10;

/// Which page an in-flight `folder.list` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    /// The listing from the start: it replaces what is shown.
    First,
    /// The page after the last one: it is appended.
    Next,
}

/// One server's place in its library and what is loaded of it.
#[derive(Debug)]
pub struct BrowseState {
    /// Library-relative folder; `""` is the root.
    path: String,
    folders: Vec<String>,
    items: Vec<MediaItem>,
    next_cursor: Option<String>,
    /// The `folder.list` waited for. Only one is in flight at a time.
    listing: Option<(RequestId, Page)>,
    /// Why the last `folder.list` failed, and which page it was; cleared by the next success.
    error: Option<(ClientError, Page)>,
    selected: Option<ItemId>,
    /// The item shown full screen: opened from the grid (double-click, or Enter on the
    /// selection), closed with Esc.
    opened: Option<ItemId>,
    /// The detail pane's edits: on their way, waiting to be sent again, failed.
    edits: Edits,
    /// The `library.info` waited for.
    info: Option<RequestId>,
    next_info: Instant,
}

impl Default for BrowseState {
    fn default() -> Self {
        BrowseState::new()
    }
}

impl BrowseState {
    /// At the root, with nothing loaded or asked for yet.
    pub fn new() -> BrowseState {
        BrowseState {
            path: String::new(),
            folders: Vec::new(),
            items: Vec::new(),
            next_cursor: None,
            listing: None,
            error: None,
            selected: None,
            opened: None,
            edits: Edits::default(),
            info: None,
            next_info: Instant::now() + IDLE_POLL,
        }
    }

    /// The folder shown, library-relative; `""` is the root.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The subfolders of [`path`](Self::path), by name.
    pub fn folders(&self) -> &[String] {
        &self.folders
    }

    /// The items loaded so far, in the server's order.
    pub fn items(&self) -> &[MediaItem] {
        &self.items
    }

    pub fn selected(&self) -> Option<&ItemId> {
        self.selected.as_ref()
    }

    /// The item shown full screen, if the viewer is open.
    pub fn opened(&self) -> Option<&ItemId> {
        self.opened.as_ref()
    }

    /// Select `id` (an item of the listing; anything else is ignored).
    pub fn select(&mut self, id: &ItemId) {
        if self.items.iter().any(|i| i.id == *id) {
            self.selected = Some(id.clone());
        }
    }

    /// The selected item, as listed.
    pub fn selected_item(&self) -> Option<&MediaItem> {
        let id = self.selected.as_ref()?;
        self.items.iter().find(|i| i.id == *id)
    }

    /// What the detail pane shows for the selected item.
    pub fn detail(&self) -> Option<DetailView<'_>> {
        Some(DetailView::new(self.selected_item()?, &self.edits))
    }

    /// Send `edit` of the selected item. Returns whether it was sent: not when nothing is
    /// selected, the item is read-only, or that field's last edit is still on its way.
    pub fn edit(&mut self, session: &mut Session, edit: Edit) -> bool {
        let Some(id) = self.selected.clone() else {
            return false;
        };
        let Some(item) = self.items.iter().find(|i| i.id == id) else {
            return false;
        };
        self.edits.send(session, item, edit)
    }

    /// Show `id` full screen (and select it).
    pub fn open_item(&mut self, id: &ItemId) {
        if self.items.iter().any(|i| i.id == *id) {
            self.selected = Some(id.clone());
            self.opened = Some(id.clone());
        }
    }

    /// Close the full-screen view; the selection stays.
    pub fn close_viewer(&mut self) {
        self.opened = None;
    }

    /// The opened item's place in the listing.
    pub(crate) fn opened_index(&self) -> Option<usize> {
        let id = self.opened.as_ref()?;
        self.items.iter().position(|i| i.id == *id)
    }

    /// Move the full-screen view `delta` items on (negative: back), staying within the
    /// listing; the selection follows. Near the end, the next page is fetched.
    pub fn step(&mut self, session: &mut Session, delta: isize) {
        let Some(index) = self.opened_index() else {
            return;
        };
        let last = self.items.len() - 1;
        let to = index.saturating_add_signed(delta).min(last);
        let id = self.items[to].id.clone();
        self.selected = Some(id.clone());
        self.opened = Some(id);
        if to + PREFETCH_ITEMS >= self.items.len() {
            self.load_more(session);
        }
    }

    /// Why the last page could not be loaded.
    pub fn error(&self) -> Option<&ClientError> {
        self.error.as_ref().map(|(e, _)| e)
    }

    /// Whether a page is on its way.
    pub fn is_loading(&self) -> bool {
        self.listing.is_some()
    }

    /// Whether more pages remain to be fetched.
    pub fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    /// Go to `path` and fetch its first page. What was shown is cleared at once, and a page
    /// still on its way for the folder left is ignored when it comes.
    pub fn open(&mut self, session: &mut Session, path: impl Into<String>) {
        self.path = path.into();
        self.folders.clear();
        self.items.clear();
        self.next_cursor = None;
        self.error = None;
        self.selected = None;
        self.opened = None;
        self.request_page(session, None, Page::First);
    }

    /// Fetch the folder shown again from the start; what is shown stays until it arrives.
    pub fn reload(&mut self, session: &mut Session) {
        self.error = None;
        self.request_page(session, None, Page::First);
    }

    /// Fetch the next page, unless one is on its way, there is none, or the last one failed.
    pub fn load_more(&mut self, session: &mut Session) {
        if self.listing.is_some() || self.error.is_some() {
            return;
        }
        if let Some(cursor) = self.next_cursor.clone() {
            self.request_page(session, Some(cursor), Page::Next);
        }
    }

    /// Ask `library.info` now (unless it is already asked). The session compares the answer
    /// with what it knew: a new epoch, a scan that ended or a new item count between scans
    /// makes the listing be fetched again, another library fails the session.
    pub fn refresh_library(&mut self, session: &mut Session) {
        if self.info.is_none() {
            self.info = Some(session.request(Request::LibraryInfo));
        }
    }

    fn request_page(&mut self, session: &mut Session, cursor: Option<String>, page: Page) {
        let id = session.request(Request::FolderList(FolderListParams {
            path: self.path.clone(),
            cursor,
            limit: Some(PAGE_SIZE),
        }));
        // A page asked earlier is now forgotten: its reply will not match.
        self.listing = Some((id, page));
    }

    /// Take `reply` if it is one this state waits for.
    pub fn handle(&mut self, reply: &Reply) {
        if self.edits.handle(reply, &mut self.items) {
            return;
        }
        if self.info == Some(reply.id) {
            self.info = None;
            return;
        }
        let Some((id, page)) = self.listing else {
            return;
        };
        if id != reply.id {
            return;
        }
        self.listing = None;
        match &reply.result {
            Ok(Response::FolderList(listing)) => {
                match page {
                    Page::First => {
                        self.folders = listing.folders.clone();
                        self.items = listing.items.clone();
                        if let Some(sel) = &self.selected
                            && !self.items.iter().any(|i| &i.id == sel)
                        {
                            self.selected = None;
                        }
                        if let Some(open) = &self.opened
                            && !self.items.iter().any(|i| &i.id == open)
                        {
                            self.opened = None;
                        }
                    }
                    // Later pages carry no folders.
                    Page::Next => self.items.extend(listing.items.iter().cloned()),
                }
                self.next_cursor = listing.next_cursor.clone();
                self.error = None;
            }
            Ok(other) => {
                let e = ClientError::Protocol(format!("folder.list answered with {other:?}"));
                self.error = Some((e, page));
            }
            Err(e) => self.error = Some((e.clone(), page)),
        }
    }

    /// Per-frame upkeep: fetch the listing again (keeping it on screen until the new first
    /// page arrives) when the session says the library changed (a new epoch, a scan that
    /// ended, a new item count between scans; see
    /// [`Session::take_refetch_listing`]), and ask
    /// `library.info` again on schedule (often while the server scans). Asks egui to wake
    /// for the next check.
    pub fn tick(&mut self, session: &mut Session, ctx: &egui::Context) {
        self.edits
            .tick(session, ctx, self.selected.as_ref(), &self.items);
        if session.take_refetch_listing() {
            self.reload(session);
        }
        let SessionState::Ready { library } = session.state() else {
            return;
        };
        let interval = if library.scanning {
            SCANNING_POLL
        } else {
            IDLE_POLL
        };
        let now = Instant::now();
        // A shorter interval (a scan began) takes effect at once.
        self.next_info = self.next_info.min(now + interval);
        if self.info.is_none() && now >= self.next_info {
            self.refresh_library(session);
            self.next_info = now + interval;
        }
        ctx.request_repaint_after(self.next_info.saturating_duration_since(now));
    }

    /// The library screen's body: where the user is, the scan's progress, the subfolders and
    /// the grid. Returns the item opened this frame (double-clicked, or Enter on the
    /// selection); it is also kept as [`opened`](Self::opened).
    pub fn ui(&mut self, ui: &mut egui::Ui, session: &mut Session) -> Option<ItemId> {
        let mut go_to = None;
        egui::Panel::top("asteria_photos_location").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                go_to = self.breadcrumb_ui(ui);
                if let SessionState::Ready { library } = session.state()
                    && library.scanning
                {
                    ui.separator();
                    ui.spinner();
                    ui.weak(tr!(
                        ui,
                        "asteria-photos-library-scanning",
                        count = library.item_count
                    ));
                }
            });
            if !self.folders.is_empty() {
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("asteria_photos_folders")
                    .max_height(96.0)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for name in &self.folders {
                                let icon = icons::image(ui, icons::folder(), 14.0);
                                if ui
                                    .add(egui::Button::image_and_text(icon, name.as_str()))
                                    .clicked()
                                {
                                    go_to = Some(join(&self.path, name));
                                }
                            }
                        });
                    });
            }
            ui.add_space(2.0);
        });
        if let Some(path) = go_to {
            self.open(session, path);
        }
        let mut opened = None;
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some((e, page)) = self.error.clone() {
                ui.horizontal_wrapped(|ui| {
                    let reason = error_text(ui.ctx(), &e);
                    let label = ui.colored_label(
                        ui.visuals().error_fg_color,
                        tr!(ui, "asteria-photos-library-failed", reason = reason),
                    );
                    if let Some(detail) = error_detail(&e) {
                        label.on_hover_text(detail);
                    }
                    if ui.button(tr!(ui, "asteria-photos-library-retry")).clicked() {
                        self.error = None;
                        match page {
                            Page::First => self.reload(session),
                            Page::Next => self.load_more(session),
                        }
                    }
                });
            } else if self.items.is_empty() && self.listing.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr!(ui, "asteria-photos-library-loading"));
                });
            } else if self.items.is_empty() && self.folders.is_empty() {
                ui.weak(tr!(ui, "asteria-photos-library-empty"));
            }
            if self.items.is_empty() {
                return;
            }
            let salt = (
                "asteria_photos_grid",
                session.generation(),
                self.path.as_str(),
            );
            let r = ThumbGrid::new(&self.items)
                .selected(self.selected.clone())
                .id_salt(salt)
                .show(ui, session.images());
            if let Some(id) = r.selected {
                self.selected = Some(id);
            }
            if let Some(id) = r.double_clicked {
                self.opened = Some(id.clone());
                opened = Some(id);
            }
            let near_end = r
                .last_visible
                .is_some_and(|last| last + PREFETCH_ROWS * r.columns >= self.items.len());
            if near_end {
                self.load_more(session);
            }
        });
        opened
    }

    /// The detail pane for the selected item, or a word that nothing is selected.
    pub(crate) fn detail_ui(&mut self, ui: &mut egui::Ui, session: &mut Session) {
        let found = self
            .selected
            .as_ref()
            .and_then(|id| self.items.iter().find(|i| i.id == *id));
        let Some(item) = found else {
            ui.weak(tr!(ui, "asteria-photos-detail-none"));
            return;
        };
        if crate::detail::ui(ui, item, &mut self.edits, session) {
            self.opened = Some(item.id.clone());
        }
    }

    /// The path as clickable steps, the root first. Returns the step clicked.
    fn breadcrumb_ui(&self, ui: &mut egui::Ui) -> Option<String> {
        let mut go_to = None;
        let here = self.path.is_empty();
        if ui
            .selectable_label(here, tr!(ui, "asteria-photos-library-root"))
            .clicked()
            && !here
        {
            go_to = Some(String::new());
        }
        let mut prefix = String::new();
        for part in self.path.split('/').filter(|p| !p.is_empty()) {
            prefix = join(&prefix, part);
            ui.add(icons::image(ui, icons::chevron_right(), 12.0));
            let here = prefix == self.path;
            if ui.selectable_label(here, part).clicked() && !here {
                go_to = Some(prefix.clone());
            }
        }
        go_to
    }
}

/// `name` inside the folder `path` (`""` being the root).
fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::join;

    #[test]
    fn joins_below_the_root_and_below_a_folder() {
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", "b"), "a/b");
    }
}
