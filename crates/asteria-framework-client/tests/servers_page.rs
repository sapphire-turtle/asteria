//! `ServersPage`: the form logic without clicking, and the whole page driven headlessly
//! through `egui::Context::run`, including the connection test against a stub server.

mod common;

use std::time::{Duration, Instant};

use asteria_framework_client::errors::error_text;
use asteria_framework_client::i18n;
use asteria_framework_client::profiles::{Profiles, Token};
use asteria_framework_client::rpc::ClientError;
use asteria_framework_client::session::SessionState;
use asteria_framework_client::settings::servers::{FormError, ServersChange, ServersPage};
use corundum_framework::egui;
use corundum_framework::gui::settings::SettingsSection;

use common::{DEADLINE, StubResponse, library_info, photos_client, photos_server_info, serve};

fn page_with(n: usize) -> (ServersPage, Profiles) {
    let mut profiles = Profiles::default();
    for i in 0..n {
        profiles
            .add(format!("s{i}"), "http://localhost:1", Token::new("t"))
            .unwrap();
    }
    (ServersPage::new(&profiles, photos_client()), profiles)
}

fn fill(page: &mut ServersPage, name: &str, url: &str, token: &str) {
    let form = page.form_mut().unwrap();
    form.name = name.into();
    form.url = url.into();
    form.token = token.into();
}

fn frame(ctx: &egui::Context, page: &mut ServersPage) {
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| page.ui(ui));
    // There is no renderer to hand the texture uploads to.
    output.textures_delta.clear();
}

#[test]
fn adding_gives_the_entered_values() {
    let (mut page, _) = page_with(0);
    page.begin_add();
    fill(&mut page, " Home ", " http://nas.local:8080 ", "secret");
    assert!(page.dirty());
    page.submit().unwrap();
    match page.take_change() {
        Some(ServersChange::Added { name, url, token }) => {
            assert_eq!(name, "Home");
            assert_eq!(url.as_str(), "http://nas.local:8080/");
            assert_eq!(token.expose(), "secret");
        }
        other => panic!("{other:?}"),
    }
    assert!(page.form().is_none());
    assert!(!page.dirty());
}

#[test]
fn invalid_input_gives_no_change() {
    let (mut page, _) = page_with(0);
    page.begin_add();
    fill(&mut page, "x", "ftp://nas", "");
    assert_eq!(page.submit(), Err(FormError::InvalidUrl));
    fill(&mut page, "  ", "http://nas", "");
    assert_eq!(page.submit(), Err(FormError::NameEmpty));
    assert_eq!(page.take_change(), None);
    assert!(page.form().is_some());
}

#[test]
fn editing_updates_the_same_profile() {
    let (mut page, profiles) = page_with(2);
    let id = profiles.servers[1].id;
    page.begin_edit(id);
    assert!(!page.dirty());
    page.form_mut().unwrap().name = "renamed".into();
    assert!(page.dirty());
    page.submit().unwrap();
    match page.take_change() {
        Some(ServersChange::Updated(p)) => {
            assert_eq!((p.id, p.name.as_str()), (id, "renamed"));
            assert_eq!(p.token.expose(), "t");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn activate_and_delete() {
    let (mut page, mut profiles) = page_with(2);
    let (a, b) = (profiles.servers[0].id, profiles.servers[1].id);
    profiles.set_active(Some(a)).unwrap();
    page.set_profiles(&profiles);
    page.activate(a);
    assert_eq!(page.take_change(), None, "already active");
    page.activate(b);
    assert_eq!(page.take_change(), Some(ServersChange::Activate(b)));
    // Deleting asks first.
    page.ask_delete(a);
    assert_eq!(page.take_change(), None);
    page.confirm_delete();
    assert_eq!(page.take_change(), Some(ServersChange::Removed(a)));
}

#[test]
fn set_profiles_closes_a_form_for_a_removed_server() {
    let (mut page, mut profiles) = page_with(1);
    let id = profiles.servers[0].id;
    page.begin_edit(id);
    profiles.remove(id);
    page.set_profiles(&profiles);
    assert!(page.form().is_none());
}

#[test]
fn error_texts_name_the_failure_without_a_token() {
    let ctx = egui::Context::default();
    i18n::register(&ctx);
    let t = error_text(&ctx, &ClientError::Unauthorized);
    assert!(!t.is_empty() && !t.contains("asteria-client-"), "{t}");
}

#[test]
fn page_renders_and_labels() {
    let ctx = egui::Context::default();
    i18n::register(&ctx);
    let (mut page, profiles) = page_with(2);
    assert_eq!(page.label(&ctx), "Servers");
    frame(&ctx, &mut page);
    page.begin_edit(profiles.servers[0].id);
    frame(&ctx, &mut page);
    page.begin_add();
    frame(&ctx, &mut page);
}

fn run_until_settled(ctx: &egui::Context, page: &mut ServersPage) -> SessionState {
    let start = Instant::now();
    loop {
        frame(ctx, page);
        match page.test_state() {
            Some(SessionState::Connecting) => {}
            Some(other) => return other.clone(),
            None => panic!("no test running"),
        }
        assert!(start.elapsed() < DEADLINE, "test never settled");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn connection_test_reports_the_library_then_goes_away_on_edit() {
    let addr = serve(|req| match req.rpc() {
        Some((id, m)) if m == "server.info" => StubResponse::rpc_result(id, photos_server_info()),
        Some((id, _)) => StubResponse::rpc_result(id, library_info("lib-1")),
        None => StubResponse::new(404, Vec::new()),
    });
    let ctx = egui::Context::default();
    i18n::register(&ctx);
    let (mut page, _) = page_with(0);
    page.begin_add();
    fill(&mut page, "stub", &format!("http://{addr}"), "tok");
    page.start_test(&ctx);
    match run_until_settled(&ctx, &mut page) {
        SessionState::Ready { library } => assert_eq!(library.owner, "owner"),
        other => panic!("{other:?}"),
    }
    // Nothing was saved by testing.
    assert_eq!(page.take_change(), None);
    // Changing the form drops the result: a frame cannot see a text edit here, so close.
    page.close_form();
    assert!(page.test_state().is_none());
}

#[test]
fn connection_test_reports_an_unreachable_server() {
    let ctx = egui::Context::default();
    i18n::register(&ctx);
    let (mut page, _) = page_with(0);
    page.begin_add();
    // Port 0 never accepts a connection (a port freed by the test could be taken by another
    // test's stub meanwhile).
    fill(&mut page, "dead", "http://127.0.0.1:0", "tok");
    page.start_test(&ctx);
    assert!(matches!(
        run_until_settled(&ctx, &mut page),
        SessionState::Failed(ClientError::Unreachable(_))
    ));
}

#[test]
fn technical_details_stay_out_of_the_sentence() {
    use asteria_framework_client::errors::error_detail;
    let ctx = egui::Context::default();
    i18n::register(&ctx);
    for e in [
        ClientError::Protocol("weird-detail".into()),
        ClientError::Rpc {
            code: -32000,
            message: "weird-detail".into(),
        },
        ClientError::Io("weird-detail".into()),
    ] {
        let t = error_text(&ctx, &e);
        assert!(
            !t.contains("weird-detail") && !t.contains("asteria-client-"),
            "{t}"
        );
        assert!(error_detail(&e).unwrap().contains("weird-detail"));
    }
    for e in [
        ClientError::NotFound,
        ClientError::Unprocessable,
        ClientError::LibraryChanged,
        ClientError::Retry {
            after: Duration::from_secs(2),
        },
    ] {
        let t = error_text(&ctx, &e);
        assert!(!t.is_empty() && !t.contains("asteria-client-"), "{t}");
        assert_eq!(error_detail(&e), None);
    }
}

#[test]
fn debug_output_never_shows_the_token() {
    const TOKEN: &str = "very-secret-token-value";
    // Adding: the form, then the change it gives.
    let (mut page, _) = page_with(0);
    page.begin_add();
    fill(&mut page, "Home", "http://nas.local:7810", TOKEN);
    let form = format!("{:?}", page.form().unwrap());
    assert!(!form.contains(TOKEN), "{form}");
    page.submit().unwrap();
    let added = page.take_change().unwrap();
    assert!(matches!(added, ServersChange::Added { .. }));
    let added = format!("{added:?}");
    assert!(!added.contains(TOKEN), "{added}");
    // Editing: the form, then the change it gives.
    let (mut page, profiles) = page_with(1);
    page.begin_edit(profiles.servers[0].id);
    page.form_mut().unwrap().token = TOKEN.into();
    let form = format!("{:?}", page.form().unwrap());
    assert!(!form.contains(TOKEN), "{form}");
    page.submit().unwrap();
    let updated = page.take_change().unwrap();
    assert!(matches!(updated, ServersChange::Updated(_)));
    let updated = format!("{updated:?}");
    assert!(!updated.contains(TOKEN), "{updated}");
}
