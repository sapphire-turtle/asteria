//! Uploading from the app: files dropped on it go to the server's Inbox, and the screens
//! show where they are. Driven headlessly against a real server.

mod support;

use std::path::{Path, PathBuf};

use asteria_framework_client::upload::UploadSnapshot;
use asteria_framework_client::upload::queue::{GroupId, GroupState, QueueSummary, QueuedGroup};
use asteria_photos::app::{PhotosApp, Screen};
use asteria_photos::uploads::{Actions, actions, status_counts};
use corundum_framework::egui;

use support::server::TestServer;
use support::*;

fn red() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/asteria-framework/tests/fixtures/red.jpg")
}

/// A JPEG that differs from red.jpg (and from every other `n`) by trailing bytes.
fn jpeg(path: &Path, n: u8) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = std::fs::read(red()).unwrap();
    bytes.extend_from_slice(&[n; 16]);
    std::fs::write(path, bytes).unwrap();
}

fn snapshot(app: &PhotosApp) -> Option<UploadSnapshot> {
    Some(app.connection()?.session().uploads()?.snapshot())
}

fn start(server: &TestServer) -> (Fixture, egui::Context, PhotosApp) {
    let (fx, _) = fixture_with_tokens(&[("S", server.addr, &server.token)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    // The engine starts once the handshake has told the library.
    run_until(
        &ctx,
        &mut app,
        "the upload engine",
        |_| {},
        |app| snapshot(app).is_some(),
    );
    (fx, ctx, app)
}

#[test]
fn dropping_a_folder_uploads_it() {
    let server = TestServer::start(&[], |_| {});
    let (_fx, ctx, mut app) = start(&server);
    let src = tempfile::tempdir().unwrap();
    jpeg(&src.path().join("trip/a.jpg"), 1);
    jpeg(&src.path().join("trip/b.jpg"), 2);

    app.drop_files(vec![src.path().join("trip")]);
    assert_eq!(app.current_screen(), Some(Screen::Uploads));
    run_until(
        &ctx,
        &mut app,
        "both uploads",
        |_| {},
        |app| snapshot(app).is_some_and(|s| s.summary.done == 2),
    );
    assert!(server.root.join("Inbox/trip/a.jpg").is_file());
    assert!(server.root.join("Inbox/trip/b.jpg").is_file());
}

#[test]
fn duplicate_row_offers_keep_and_discard() {
    let server = TestServer::start(&["existing.jpg"], |_| {});
    let (_fx, ctx, mut app) = start(&server);
    let src = tempfile::tempdir().unwrap();
    std::fs::copy(red(), src.path().join("again.jpg")).unwrap();

    app.drop_files(vec![src.path().join("again.jpg")]);
    run_until(
        &ctx,
        &mut app,
        "the duplicate to be found",
        |_| {},
        |app| snapshot(app).is_some_and(|s| s.summary.awaiting == 1),
    );
    let snap = snapshot(&app).unwrap();
    let group = &snap.groups[0];
    let GroupState::AwaitingDecision { existing } = &group.state else {
        panic!("{:?}", group.state)
    };
    assert!(existing.iter().any(|p| p.ends_with("existing.jpg")));
    let offered = actions(&group.state);
    assert!(offered.keep && offered.discard && !offered.retry);
    assert!(offered.cancel);

    // The screen draws it, and Keep puts it in the Inbox.
    frame(&ctx, &mut app);
    let uploads = app.connection().unwrap().session().uploads().unwrap();
    assert!(uploads.decide(group.id, true));
    run_until(
        &ctx,
        &mut app,
        "the kept copy",
        |_| {},
        |app| snapshot(app).is_some_and(|s| s.summary.done == 1),
    );
    assert!(server.root.join("Inbox/again.jpg").is_file());
}

#[test]
fn actions_follow_the_state() {
    let failed = GroupState::Failed { reason: "x".into() };
    assert_eq!(
        actions(&failed),
        Actions {
            retry: true,
            cancel: true,
            ..Actions::default()
        }
    );
    assert_eq!(actions(&GroupState::Discarded), Actions::default());
    assert_eq!(
        actions(&GroupState::Done {
            placed: vec![],
            warnings: vec![]
        }),
        Actions::default()
    );
    assert!(actions(&GroupState::Hashing).cancel);
}

#[test]
fn drop_while_disconnected_is_refused() {
    // No server registered at all.
    let dir = tempfile::tempdir().unwrap();
    let paths = asteria_photos::app::AppPaths {
        servers: dir.path().join("servers.toml"),
        cache: None,
        data: Some(dir.path().join("data")),
    };
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(paths, &ctx);
    let before = app.current_screen();
    app.drop_files(vec![dir.path().join("x.jpg")]);
    assert_eq!(app.current_screen(), before);
    frame(&ctx, &mut app);
    assert!(app.notice().is_some());
}

fn group(state: GroupState) -> QueuedGroup {
    QueuedGroup {
        id: GroupId(grain_id::GrainId::random()),
        files: vec![],
        state,
        sent: 0,
    }
}

#[test]
fn status_bar_counts_pending() {
    let done = || GroupState::Done {
        placed: vec![],
        warnings: vec![],
    };
    let waiting = || GroupState::AwaitingDecision { existing: vec![] };
    let groups = vec![
        group(done()),
        group(done()),
        group(waiting()),
        group(GroupState::Hashing),
        group(GroupState::Discarded),
    ];
    let summary = QueueSummary {
        total: 5,
        done: 2,
        awaiting: 1,
        ..Default::default()
    };
    // Done over all, and what waits for a decision.
    assert_eq!(status_counts(&groups, &summary), Some((2, 5, 1)));
    // Nothing unfinished: nothing shown.
    let finished = vec![group(done()), group(GroupState::Discarded)];
    let summary = QueueSummary {
        total: 2,
        done: 1,
        ..Default::default()
    };
    assert_eq!(status_counts(&finished, &summary), None);
    assert_eq!(status_counts(&[], &QueueSummary::default()), None);
}

#[test]
fn i18n_keys_exist_in_both_languages() {
    use corundum_framework::gui::i18n::check;
    let r = &asteria_photos::i18n::RESOURCES;
    assert!(check::key_difference(r, "en", "ja").is_empty());
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    assert_eq!(
        check::unknown_ids(r, "en", &src, "asteria-photos-", &[]),
        Vec::<String>::new()
    );
    assert_eq!(
        check::unknown_ids(r, "ja", &src, "asteria-photos-", &[]),
        Vec::<String>::new()
    );
}
