//! Settings pages added from outside the app (the desktop's "Local server"): listed after
//! the servers and before playback, and drawn when chosen.

mod support;

use std::cell::Cell;
use std::rc::Rc;

use asteria_common_client::settings::ExtraSettingsPage;
use asteria_music_gui::app::{AppPaths, MusicApp, Screen};
use corundum_framework::egui;

use support::player;

/// A page that counts how often it was drawn.
struct Counted {
    drawn: Rc<Cell<usize>>,
}

impl ExtraSettingsPage for Counted {
    fn label(&self, _: &egui::Context) -> String {
        "Extra page".into()
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.drawn.set(self.drawn.get() + 1);
        ui.label("inside the extra page");
    }
}

/// One frame in which `events` arrive; what it put out.
fn frame(ctx: &egui::Context, app: &mut MusicApp, events: Vec<egui::Event>) -> egui::FullOutput {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            events,
            ..Default::default()
        },
        |ui| app.show(ui),
    );
    // There is no renderer to hand the texture uploads to.
    output.textures_delta.clear();
    output
}

/// Where the node labelled exactly `label` is, if the frame has one.
fn find(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
    let update = output.platform_output.accesskit_update.as_ref()?;
    update.nodes.iter().find_map(|(_, node)| {
        let bounds = node.bounds()?;
        (node.label() == Some(label)).then(|| {
            egui::pos2(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            )
        })
    })
}

#[test]
fn an_extra_page_is_listed_between_servers_and_playback() {
    let dir = tempfile::tempdir().unwrap();
    // No servers: the app opens on Settings, at the servers page.
    let paths = AppPaths {
        servers: dir.path().join("config/servers.toml"),
        playback: dir.path().join("config/playback.toml"),
        cache: None,
        data: Some(dir.path().join("data")),
    };
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let drawn = Rc::new(Cell::new(0));
    let (player, settings) = player(&ctx);
    let mut app =
        MusicApp::new(paths, &ctx, player, settings).with_extra_settings(vec![Box::new(Counted {
            drawn: drawn.clone(),
        })]);
    assert_eq!(app.current_screen(), Some(Screen::Settings));

    let output = frame(&ctx, &mut app, Vec::new());
    let at = find(&output, "Extra page").expect("the extra page is listed");
    let playback = find(&output, "Playback").expect("playback is listed");
    assert!(at.y < playback.y, "the extra page comes before playback");
    assert_eq!(
        drawn.get(),
        0,
        "the servers page is open, not the extra one"
    );

    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    frame(
        &ctx,
        &mut app,
        vec![egui::Event::PointerMoved(at), button(true)],
    );
    frame(&ctx, &mut app, vec![button(false)]);
    frame(&ctx, &mut app, Vec::new());
    assert!(drawn.get() > 0, "choosing the extra page draws it");
}

#[test]
fn the_offline_page_is_listed_after_playback() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths {
        servers: dir.path().join("config/servers.toml"),
        playback: dir.path().join("config/playback.toml"),
        cache: None,
        data: Some(dir.path().join("data")),
    };
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let (player, settings) = player(&ctx);
    let mut app = MusicApp::new(paths, &ctx, player, settings);
    let output = frame(&ctx, &mut app, Vec::new());
    let offline = find(&output, "Offline").expect("the offline page is listed");
    let playback = find(&output, "Playback").expect("playback is listed");
    assert!(playback.y < offline.y, "offline comes after playback");
}
