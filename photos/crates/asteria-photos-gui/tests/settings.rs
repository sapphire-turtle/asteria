//! Settings pages added from outside the app (the desktop's "Local server"): listed after
//! the servers, and drawn when chosen.

mod support;

use std::cell::Cell;
use std::rc::Rc;

use asteria_common_client::settings::ExtraSettingsPage;
use asteria_photos_gui::app::{AppPaths, PhotosApp, Screen};
use corundum_framework::egui;

use support::{frame, frame_with};

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
fn an_extra_page_is_listed_and_drawn_when_chosen() {
    let dir = tempfile::tempdir().unwrap();
    // No servers: the app opens on Settings, at the servers page.
    let paths = AppPaths {
        servers: dir.path().join("config/servers.toml"),
        display: dir.path().join("config/display.toml"),
        cache: None,
        data: None,
    };
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let drawn = Rc::new(Cell::new(0));
    let mut app = PhotosApp::new(paths, &ctx).with_extra_settings(vec![Box::new(Counted {
        drawn: drawn.clone(),
    })]);
    assert_eq!(app.current_screen(), Some(Screen::Settings));

    let output = frame_with(&ctx, &mut app, Vec::new());
    let at = find(&output, "Extra page").expect("the extra page is listed");
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
    frame_with(
        &ctx,
        &mut app,
        vec![egui::Event::PointerMoved(at), button(true)],
    );
    frame_with(&ctx, &mut app, vec![button(false)]);
    frame(&ctx, &mut app);
    assert!(drawn.get() > 0, "choosing the extra page draws it");
}

#[test]
fn display_and_offline_are_listed_after_the_extra_pages() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths {
        servers: dir.path().join("config/servers.toml"),
        display: dir.path().join("config/display.toml"),
        cache: None,
        data: Some(dir.path().join("data")),
    };
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = PhotosApp::new(paths, &ctx).with_extra_settings(vec![Box::new(Counted {
        drawn: Rc::default(),
    })]);
    let output = frame_with(&ctx, &mut app, Vec::new());
    let extra = find(&output, "Extra page").expect("the extra page is listed");
    let display = find(&output, "Display").expect("the display page is listed");
    let offline = find(&output, "Offline").expect("the offline page is listed");
    assert!(extra.y < display.y, "display comes after the extra pages");
    assert!(display.y < offline.y, "offline comes after display");
}
