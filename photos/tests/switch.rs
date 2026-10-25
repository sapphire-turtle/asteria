//! Switching servers: the app driven headlessly through `egui::Context::run`, against stub
//! servers.

mod support;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asteria_framework_client::session::SessionState;
use asteria_photos::app::{AppPaths, PhotosApp, Screen};
use corundum_framework::egui;

use support::common::{DEADLINE, StubResponse, library_info, photos_server_info, serve};
use support::*;

#[test]
fn switching_drops_pending_results() {
    let (asked_tx, asked) = mpsc::channel::<()>();
    let (release_tx, release) = mpsc::channel::<()>();
    let release = Mutex::new(release);
    let (written_tx, written) = mpsc::channel::<()>();
    let written_tx = Mutex::new(written_tx);
    // A holds its folder.list until the test lets it go.
    let (a, _) = stub("lib-a", move |_, id| {
        asked_tx.send(()).unwrap();
        release.lock().unwrap().recv().unwrap();
        let tx = written_tx.lock().unwrap().clone();
        let mut resp = StubResponse::rpc_result(id, listing(&["a-folder"], &["a-item"]));
        resp.after_write = Some(Box::new(move || {
            let _ = tx.send(());
        }));
        resp
    });
    // B holds its own until A's answer is out, so A's answer comes while B's request (with
    // the same request id, both sessions' first) is still open.
    let (b_asked_tx, b_asked) = mpsc::channel::<()>();
    let (b_release_tx, b_release) = mpsc::channel::<()>();
    let b_release = Mutex::new(b_release);
    let (b, _) = stub("lib-b", move |_, id| {
        b_asked_tx.send(()).unwrap();
        b_release.lock().unwrap().recv().unwrap();
        StubResponse::rpc_result(id, listing(&[], &["b-item"]))
    });
    let (fx, ids) = fixture(&[("A", a), ("B", b)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    assert_eq!(connected_to(&app), Some(ids[0]));

    // A's listing is on its way when the switch happens.
    run_until(
        &ctx,
        &mut app,
        "A's folder.list",
        |_| {},
        |_| asked.try_recv().is_ok(),
    );
    app.switch_to(ids[1]);
    assert_eq!(connected_to(&app), Some(ids[1]));
    let no_a = |app: &PhotosApp| {
        let items = item_ids(app);
        assert!(!items.contains(&"a-item".to_string()), "{items:?}");
        let folders = app.connection().unwrap().browse().folders().to_vec();
        assert!(folders.is_empty(), "{folders:?}");
    };
    run_until(&ctx, &mut app, "B's folder.list", no_a, |_| {
        b_asked.try_recv().is_ok()
    });

    // A answers now, after the switch: nothing of it may show.
    release_tx.send(()).unwrap();
    written.recv_timeout(DEADLINE).unwrap();
    let watch = Instant::now();
    while watch.elapsed() < Duration::from_millis(300) {
        frame(&ctx, &mut app);
        no_a(&app);
        assert_eq!(connected_to(&app), Some(ids[1]));
        std::thread::sleep(Duration::from_millis(5));
    }
    // Then B's own answer shows.
    b_release_tx.send(()).unwrap();
    run_until(&ctx, &mut app, "B's listing", no_a, |app| {
        item_ids(app) == ["b-item"]
    });
    wait_saved(&fx.paths.servers, ids[1]);
}

#[test]
fn switching_resets_location() {
    let (a, _) = stub("lib-a", |req, id| {
        let result = match params(req)["path"].as_str().unwrap() {
            "" => listing(&["sub"], &["a-root"]),
            _ => listing(&[], &["a-sub"]),
        };
        StubResponse::rpc_result(id, result)
    });
    let b_paths = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = b_paths.clone();
    let (b, _) = stub("lib-b", move |req, id| {
        log.lock()
            .unwrap()
            .push(params(req)["path"].as_str().unwrap().to_string());
        StubResponse::rpc_result(id, listing(&[], &["b-item"]))
    });
    let (fx, ids) = fixture(&[("A", a), ("B", b)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "A's root",
        |_| {},
        |app| item_ids(app) == ["a-root"],
    );
    app.open_folder("sub");
    run_until(
        &ctx,
        &mut app,
        "A's sub",
        |_| {},
        |app| item_ids(app) == ["a-sub"],
    );
    assert_eq!(app.connection().unwrap().browse().path(), "sub");

    app.switch_to(ids[1]);
    assert_eq!(app.connection().unwrap().browse().path(), "");
    run_until(
        &ctx,
        &mut app,
        "B's root",
        |_| {},
        |app| item_ids(app) == ["b-item"],
    );
    assert_eq!(app.connection().unwrap().browse().path(), "");
    assert_eq!(*b_paths.lock().unwrap(), [""]);
}

#[test]
fn active_profile_is_restored_on_start() {
    let (a, a_seen) = stub("lib-a", |_, id| {
        StubResponse::rpc_result(id, listing(&[], &["a-item"]))
    });
    let (b, _) = stub("lib-b", |_, id| {
        StubResponse::rpc_result(id, listing(&[], &["b-item"]))
    });
    let (fx, ids) = fixture(&[("A", a), ("B", b)], 1);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    assert_eq!(app.active_profile(), Some(ids[1]));
    run_until(
        &ctx,
        &mut app,
        "B ready with its listing",
        |_| {},
        |app| {
            matches!(
                app.connection().map(|c| c.session().state()),
                Some(SessionState::Ready { .. })
            ) && item_ids(app) == ["b-item"]
        },
    );
    assert_eq!(connected_to(&app), Some(ids[1]));
    assert_eq!(app.current_screen(), Some(Screen::Library));
    assert!(a_seen.lock().unwrap().is_empty());
}

#[test]
fn with_no_servers_settings_open_on_the_servers_page() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths {
        servers: dir.path().join("servers.toml"),
        cache: None,
    };
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(paths, &ctx);
    frame(&ctx, &mut app);
    assert_eq!(app.current_screen(), Some(Screen::Settings));
    assert!(app.connection().is_none());
}

#[test]
fn a_changed_library_reconnects_once() {
    // Every library.info names a new library: the handshake's, then a different one.
    let calls = Arc::new(Mutex::new(0u32));
    let addr = serve(move |req| {
        let Some((id, method)) = req.rpc() else {
            return StubResponse::new(404, Vec::new());
        };
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" => {
                let mut n = calls.lock().unwrap();
                *n += 1;
                StubResponse::rpc_result(id, library_info(&format!("lib-{n}")))
            }
            "folder.list" => StubResponse::rpc_result(id, listing(&[], &["x"])),
            _ => StubResponse::new(404, Vec::new()),
        }
    });
    let (fx, _) = fixture(&[("A", addr)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    let ready = |app: &PhotosApp| {
        matches!(
            app.connection().map(|c| c.session().state()),
            Some(SessionState::Ready { .. })
        )
    };
    run_until(&ctx, &mut app, "the first session", |_| {}, ready);
    let first = app.connection().unwrap().session().generation();
    app.open_folder("somewhere");
    // The server's library changes: the app opens a new session, back at the root.
    app.refresh_library();
    run_until(
        &ctx,
        &mut app,
        "the reconnected session",
        |_| {},
        |app| ready(app) && app.connection().unwrap().session().generation() != first,
    );
    assert_eq!(app.connection().unwrap().browse().path(), "");
    let second = app.connection().unwrap().session().generation();
    // It changes again: this time the failure stays on screen.
    app.refresh_library();
    run_until(
        &ctx,
        &mut app,
        "the second failure",
        |_| {},
        |app| {
            matches!(
                app.connection().map(|c| c.session().state()),
                Some(SessionState::Failed(_))
            )
        },
    );
    assert_eq!(app.connection().unwrap().session().generation(), second);
}

#[test]
fn the_first_server_added_is_used_and_saved() {
    let (a, _) = stub("lib-a", |_, id| {
        StubResponse::rpc_result(id, listing(&[], &["a-item"]))
    });
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths {
        servers: dir.path().join("servers.toml"),
        cache: None,
    };
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(paths.clone(), &ctx);
    frame(&ctx, &mut app);
    let page = app.servers_page_mut();
    let form = page.form_mut().expect("the add form is open");
    form.name = "A".into();
    form.url = format!("http://{a}");
    form.token = "t".into();
    page.submit().unwrap();
    run_until(
        &ctx,
        &mut app,
        "A's listing",
        |_| {},
        |app| item_ids(app) == ["a-item"],
    );
    let id = app.active_profile().expect("the new server is in use");
    wait_saved(&paths.servers, id);
}

#[test]
fn the_library_draws_wide_and_narrow() {
    let (a, _) = stub("lib-a", |_, id| {
        StubResponse::rpc_result(id, listing(&["sub"], &["a-item"]))
    });
    let (fx, _) = fixture(&[("A", a)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "A's listing",
        |_| {},
        |app| item_ids(app) == ["a-item"],
    );
    for width in [1000.0, 400.0] {
        for _ in 0..3 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 700.0),
                    )),
                    ..Default::default()
                },
                |ui| app.show(ui),
            );
            output.textures_delta.clear();
        }
    }
}

#[test]
fn renaming_the_server_in_use_keeps_the_connection() {
    let (a, _) = stub("lib-a", |_, id| {
        StubResponse::rpc_result(id, listing(&[], &["a-item"]))
    });
    let (fx, ids) = fixture(&[("A", a)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    run_until(
        &ctx,
        &mut app,
        "A's listing",
        |_| {},
        |app| item_ids(app) == ["a-item"],
    );
    let generation = app.connection().unwrap().session().generation();
    let edit =
        |app: &mut PhotosApp,
         change: &dyn Fn(&mut asteria_framework_client::settings::servers::ServerForm)| {
            let page = app.servers_page_mut();
            page.begin_edit(ids[0]);
            change(page.form_mut().unwrap());
            page.submit().unwrap();
        };
    // A new name: the same connection, and the name is saved.
    edit(&mut app, &|f| f.name = "Renamed".into());
    frame(&ctx, &mut app);
    frame(&ctx, &mut app);
    assert_eq!(app.connection().unwrap().session().generation(), generation);
    assert_eq!(app.profiles().active().unwrap().name, "Renamed");
    let start = Instant::now();
    while asteria_framework_client::profiles::Profiles::load(&fx.paths.servers)
        .unwrap()
        .active()
        .unwrap()
        .name
        != "Renamed"
    {
        assert!(start.elapsed() < DEADLINE, "the new name was never saved");
        std::thread::sleep(Duration::from_millis(5));
    }
    // A new token: connected afresh.
    edit(&mut app, &|f| f.token = "other".into());
    frame(&ctx, &mut app);
    frame(&ctx, &mut app);
    assert_ne!(app.connection().unwrap().session().generation(), generation);
}

#[test]
fn choosing_a_server_that_stopped_answering_reconnects() {
    // Connects, then refuses `library.info` (401) while `failing` is set.
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fail = failing.clone();
    let addr = serve(move |req| {
        let Some((id, method)) = req.rpc() else {
            return StubResponse::new(404, Vec::new());
        };
        match method.as_str() {
            "server.info" => StubResponse::rpc_result(id, photos_server_info()),
            "library.info" if fail.load(std::sync::atomic::Ordering::SeqCst) => {
                StubResponse::new(401, Vec::new())
            }
            "library.info" => StubResponse::rpc_result(id, library_info("lib")),
            "folder.list" => StubResponse::rpc_result(id, listing(&[], &["x"])),
            _ => StubResponse::new(404, Vec::new()),
        }
    });
    let (fx, ids) = fixture(&[("A", addr)], 0);
    let ctx = egui::Context::default();
    let mut app = PhotosApp::new(fx.paths.clone(), &ctx);
    let generation = |app: &PhotosApp| app.connection().unwrap().session().generation();
    run_until(
        &ctx,
        &mut app,
        "the listing",
        |_| {},
        |app| item_ids(app) == ["x"],
    );
    let first = generation(&app);
    // Healthy: choosing it again keeps the connection.
    app.switch_to(ids[0]);
    assert_eq!(generation(&app), first);

    failing.store(true, std::sync::atomic::Ordering::SeqCst);
    app.refresh_library();
    run_until(
        &ctx,
        &mut app,
        "the session to show it stopped answering",
        |_| {},
        |app| app.connection().unwrap().session().degraded().is_some(),
    );
    // Still connected (the listing stays), but choosing it again starts afresh.
    assert!(matches!(
        app.connection().unwrap().session().state(),
        SessionState::Ready { .. }
    ));
    failing.store(false, std::sync::atomic::Ordering::SeqCst);
    app.switch_to(ids[0]);
    assert_ne!(generation(&app), first);
    run_until(
        &ctx,
        &mut app,
        "the listing again",
        |_| {},
        |app| item_ids(app) == ["x"],
    );
    assert!(app.connection().unwrap().session().degraded().is_none());
}
