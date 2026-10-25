//! The "Servers" settings page: the registered servers, adding, editing, deleting, choosing
//! the one in use, and a connection test.
//!
//! The page saves nothing. It works on a snapshot of the [`Profiles`] and gives each thing
//! the user did as a [`ServersChange`]; the app drains them with
//! [`take_change`](ServersPage::take_change), applies and saves them, and hands the saved
//! state back with [`set_profiles`](ServersPage::set_profiles). Until it does, the list shows
//! the old state.

use asteria_framework_api::{ClientInfo, HandshakeError};
use corundum_framework::egui;
use corundum_framework::gui::settings::SettingsSection;
use corundum_framework::gui::tr;
use grain_id::GrainId;
use url::Url;

use crate::profiles::{self, ProfileId, Profiles, ServerProfile, Token, parse_url};
use crate::rpc::ClientError;
use crate::session::{Session, SessionState};

/// One thing the user did, for the app to apply to its [`Profiles`] and save.
#[derive(Debug, Clone, PartialEq)]
pub enum ServersChange {
    /// A new server; the app gives it an id (`Profiles::add`).
    Added {
        name: String,
        url: Url,
        token: Token,
    },
    /// An existing server was edited (`Profiles::update`).
    Updated(ServerProfile),
    /// A server was deleted (`Profiles::remove`; this clears the active one if it was it).
    Removed(ProfileId),
    /// "Use this server" (`Profiles::set_active`); the app reconnects.
    Activate(ProfileId),
}

/// Why the form cannot be saved or tested yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormError {
    NameEmpty,
    InvalidUrl,
}

/// The add/edit form's fields.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerForm {
    /// `Some` when editing an existing server.
    pub editing: Option<ProfileId>,
    pub name: String,
    pub url: String,
    pub token: String,
    show_token: bool,
    /// Save was tried: show the name error too.
    submitted: bool,
}

impl std::fmt::Debug for ServerForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerForm")
            .field("editing", &self.editing)
            .field("name", &self.name)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl ServerForm {
    fn blank() -> ServerForm {
        ServerForm {
            editing: None,
            name: String::new(),
            url: String::new(),
            token: String::new(),
            show_token: false,
            submitted: false,
        }
    }

    /// The same rules as `Profiles`: a non-empty name and an http(s) URL with a host.
    pub fn validate(&self) -> Result<(String, Url), FormError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(FormError::NameEmpty);
        }
        let url = parse_url(&self.url).map_err(|_: profiles::Error| FormError::InvalidUrl)?;
        Ok((name.to_string(), url))
    }
}

/// A connection test in progress or done. Dropping it drops the session.
struct Test {
    session: Session,
}

pub struct ServersPage {
    client: ClientInfo,
    servers: Vec<ServerProfile>,
    active: Option<ProfileId>,
    form: Option<ServerForm>,
    confirm_delete: Option<ProfileId>,
    test: Option<Test>,
    change: Option<ServersChange>,
    error: Option<String>,
}

impl ServersPage {
    /// `client` is what the test's handshake announces (the app's own `client_info()`).
    pub fn new(profiles: &Profiles, client: ClientInfo) -> ServersPage {
        let mut page = ServersPage {
            client,
            servers: Vec::new(),
            active: None,
            form: None,
            confirm_delete: None,
            test: None,
            change: None,
            error: None,
        };
        page.set_profiles(profiles);
        page
    }

    /// The app saved: show this state. An open form stays (a form editing a server that is
    /// gone is closed).
    pub fn set_profiles(&mut self, profiles: &Profiles) {
        self.servers = profiles.servers.clone();
        self.active = profiles.active().map(|p| p.id);
        self.error = None;
        if let Some(id) = self.form.as_ref().and_then(|f| f.editing)
            && !self.servers.iter().any(|s| s.id == id)
        {
            self.close_form();
        }
        if let Some(id) = self.confirm_delete
            && !self.servers.iter().any(|s| s.id == id)
        {
            self.confirm_delete = None;
        }
    }

    /// The app could not save the last change: say so on the page.
    pub fn show_error(&mut self, why: impl Into<String>) {
        self.error = Some(why.into());
    }

    /// The next thing the user did, if any. Call it every frame.
    pub fn take_change(&mut self) -> Option<ServersChange> {
        self.change.take()
    }

    pub fn form(&self) -> Option<&ServerForm> {
        self.form.as_ref()
    }

    pub fn form_mut(&mut self) -> Option<&mut ServerForm> {
        self.form.as_mut()
    }

    pub fn begin_add(&mut self) {
        self.close_form();
        self.form = Some(ServerForm::blank());
    }

    pub fn begin_edit(&mut self, id: ProfileId) {
        let Some(server) = self.servers.iter().find(|s| s.id == id) else {
            return;
        };
        let form = ServerForm {
            editing: Some(id),
            name: server.name.clone(),
            url: server.url.to_string(),
            token: server.token.expose().to_string(),
            ..ServerForm::blank()
        };
        self.close_form();
        self.form = Some(form);
    }

    pub fn close_form(&mut self) {
        self.form = None;
        self.test = None;
    }

    /// Validates the form and, if it is fine, gives the change and closes the form.
    pub fn submit(&mut self) -> Result<(), FormError> {
        let Some(form) = &mut self.form else {
            return Ok(());
        };
        form.submitted = true;
        let (name, url) = form.validate()?;
        let token = Token::new(form.token.clone());
        let change = match form.editing {
            Some(id) => ServersChange::Updated(ServerProfile {
                id,
                name,
                url,
                token,
            }),
            None => ServersChange::Added { name, url, token },
        };
        self.emit(change);
        self.close_form();
        Ok(())
    }

    pub fn activate(&mut self, id: ProfileId) {
        if self.active != Some(id) && self.servers.iter().any(|s| s.id == id) {
            self.emit(ServersChange::Activate(id));
        }
    }

    pub fn ask_delete(&mut self, id: ProfileId) {
        self.confirm_delete = Some(id);
    }

    pub fn confirm_delete(&mut self) {
        if let Some(id) = self.confirm_delete.take() {
            self.emit(ServersChange::Removed(id));
        }
    }

    /// Starts the handshake against the form's server, if the form is valid. The session
    /// does its network work off this thread.
    pub fn start_test(&mut self, ctx: &egui::Context) {
        let Some(form) = &self.form else { return };
        let Ok((name, url)) = form.validate() else {
            return;
        };
        let profile = ServerProfile {
            id: form.editing.unwrap_or_else(|| ProfileId(GrainId::random())),
            name,
            url,
            token: Token::new(form.token.clone()),
        };
        self.test = Some(Test {
            session: Session::open(&profile, self.client.clone(), ctx.clone()),
        });
    }

    /// The test's state, after taking in what has arrived.
    pub fn test_state(&mut self) -> Option<&SessionState> {
        let test = self.test.as_mut()?;
        test.session.poll();
        Some(test.session.state())
    }

    fn emit(&mut self, change: ServersChange) {
        // One action per frame is all a click can make; keep the first.
        if self.change.is_none() {
            self.change = Some(change);
        }
    }

    fn list_ui(&mut self, ui: &mut egui::Ui) {
        if self.servers.is_empty() {
            ui.weak(tr!(ui, "asteria-client-servers-empty"));
        }
        let mut edit = None;
        let mut ask = None;
        let mut activate = None;
        let (mut yes, mut no) = (false, false);
        for server in &self.servers {
            let is_active = self.active == Some(server.id);
            ui.horizontal_wrapped(|ui| {
                ui.strong(&server.name);
                ui.weak(server.url.as_str());
                if is_active {
                    ui.label(tr!(ui, "asteria-client-servers-in-use"));
                }
            });
            ui.horizontal_wrapped(|ui| {
                if self.confirm_delete == Some(server.id) {
                    ui.label(tr!(
                        ui,
                        "asteria-client-servers-delete-confirm",
                        name = server.name.as_str()
                    ));
                    yes |= ui
                        .button(tr!(ui, "asteria-client-servers-delete-yes"))
                        .clicked();
                    no |= ui
                        .button(tr!(ui, "asteria-client-servers-cancel"))
                        .clicked();
                    return;
                }
                if ui
                    .add_enabled(
                        !is_active,
                        egui::Button::new(tr!(ui, "asteria-client-servers-use")),
                    )
                    .clicked()
                {
                    activate = Some(server.id);
                }
                if ui.button(tr!(ui, "asteria-client-servers-edit")).clicked() {
                    edit = Some(server.id);
                }
                if ui
                    .button(tr!(ui, "asteria-client-servers-delete"))
                    .clicked()
                {
                    ask = Some(server.id);
                }
            });
            ui.separator();
        }
        if let Some(id) = activate {
            self.activate(id);
        }
        if let Some(id) = edit {
            self.begin_edit(id);
        }
        if let Some(id) = ask {
            self.ask_delete(id);
        }
        if yes {
            self.confirm_delete();
        }
        if no {
            self.confirm_delete = None;
        }
        if ui.button(tr!(ui, "asteria-client-servers-add")).clicked() {
            self.begin_add();
        }
    }

    fn form_ui(&mut self, ui: &mut egui::Ui) {
        let Some(form) = &mut self.form else { return };
        ui.separator();
        let title = if form.editing.is_some() {
            "asteria-client-servers-form-edit"
        } else {
            "asteria-client-servers-form-add"
        };
        ui.heading(tr!(ui, title));

        let mut edited = false;
        egui::Grid::new("asteria_servers_form")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label(tr!(ui, "asteria-client-servers-name"));
                edited |= ui.text_edit_singleline(&mut form.name).changed();
                ui.end_row();

                ui.label(tr!(ui, "asteria-client-servers-url"));
                edited |= ui
                    .add(
                        egui::TextEdit::singleline(&mut form.url)
                            .hint_text("http://192.168.1.10:8080"),
                    )
                    .changed();
                ui.end_row();

                ui.label(tr!(ui, "asteria-client-servers-token"));
                ui.horizontal(|ui| {
                    edited |= ui
                        .add(egui::TextEdit::singleline(&mut form.token).password(!form.show_token))
                        .changed();
                    let toggle = if form.show_token {
                        "asteria-client-servers-token-hide"
                    } else {
                        "asteria-client-servers-token-show"
                    };
                    if ui.button(tr!(ui, toggle)).clicked() {
                        form.show_token = !form.show_token;
                    }
                });
                ui.end_row();
            });

        let valid = form.validate();
        let reason = match valid {
            Err(FormError::InvalidUrl) if !form.url.trim().is_empty() || form.submitted => {
                Some("asteria-client-servers-error-url")
            }
            Err(FormError::NameEmpty) if form.submitted => {
                Some("asteria-client-servers-error-name")
            }
            _ => None,
        };
        if let Some(id) = reason {
            ui.colored_label(ui.visuals().error_fg_color, tr!(ui, id));
        }

        let (mut save, mut test, mut cancel) = (false, false, false);
        ui.horizontal(|ui| {
            save = ui.button(tr!(ui, "asteria-client-servers-save")).clicked();
            test = ui
                .add_enabled(
                    valid.is_ok(),
                    egui::Button::new(tr!(ui, "asteria-client-servers-test")),
                )
                .clicked();
            cancel = ui
                .button(tr!(ui, "asteria-client-servers-cancel"))
                .clicked();
        });

        if edited {
            // The result belongs to what was typed before.
            self.test = None;
        }
        if cancel {
            self.close_form();
        } else if save {
            let _ = self.submit();
        } else if test {
            let ctx = ui.ctx().clone();
            self.start_test(&ctx);
        }
        self.test_ui(ui);
    }

    fn test_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(state) = self.test_state() else {
            return;
        };
        match state {
            SessionState::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tr!(ui, "asteria-client-servers-testing"));
                });
            }
            SessionState::Ready { library } => {
                ui.label(tr!(
                    ui,
                    "asteria-client-servers-test-ok",
                    count = library.item_count.to_string(),
                    owner = library.owner.as_str()
                ));
            }
            SessionState::Failed(e) => {
                let reason = error_text(&ctx, e);
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tr!(ui, "asteria-client-servers-test-failed", reason = reason),
                );
            }
        }
    }
}

/// A connection failure in words the user can act on, in the installed language. The
/// error texts never contain the token.
pub fn error_text(ctx: &egui::Context, e: &ClientError) -> String {
    match e {
        ClientError::Unreachable(detail) => tr!(
            ctx,
            "asteria-client-servers-error-unreachable",
            detail = detail.as_str()
        ),
        ClientError::Unauthorized => tr!(ctx, "asteria-client-servers-error-unauthorized"),
        ClientError::Handshake(HandshakeError::AppMismatch { .. }) => {
            tr!(ctx, "asteria-client-servers-error-app-mismatch")
        }
        ClientError::Handshake(HandshakeError::ApiMismatch { client, server }) => tr!(
            ctx,
            "asteria-client-servers-error-api-mismatch",
            client = client.to_string(),
            server = server.to_string()
        ),
        other => tr!(
            ctx,
            "asteria-client-servers-error-other",
            detail = other.to_string()
        ),
    }
}

impl SettingsSection for ServersPage {
    fn label(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-client-servers-title")
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(tr!(ui, "asteria-client-servers-title"));
        ui.separator();
        if let Some(why) = self.error.clone() {
            ui.colored_label(ui.visuals().error_fg_color, why);
        }
        self.list_ui(ui);
        self.form_ui(ui);
    }

    fn dirty(&self) -> bool {
        let Some(form) = &self.form else {
            return false;
        };
        match form
            .editing
            .and_then(|id| self.servers.iter().find(|s| s.id == id))
        {
            Some(s) => {
                form.name != s.name || form.url != s.url.as_str() || form.token != s.token.expose()
            }
            None => !(form.name.is_empty() && form.url.is_empty() && form.token.is_empty()),
        }
    }
}
