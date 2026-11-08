//! The save-for-offline toggle and the Offline page, drawn headlessly with
//! `egui::Context::run_ui`: labels are read from the accessibility tree, and a click is a
//! pointer press and release at a label's centre.

mod common;

use std::sync::Arc;

use asteria_common_client::i18n;
use asteria_common_client::offline::{
    LIMIT_CHOICES_GIB, OfflineHandle, OfflinePage, OfflineStore, PinStatus, PinTarget,
};
use asteria_common_client::profiles::Token;
use asteria_common_client::widgets::offline_toggle::{offline_status, offline_toggle, saved_mark};
use corundum_framework::egui;
use corundum_framework::gui::icons;

use common::offline::{FakeCatalog, FakeServer, library, set_pin, wait_for};

/// A handle on a temporary data directory, connected to a stub server.
struct Fixture {
    _dir: tempfile::TempDir,
    server: FakeServer,
    handle: OfflineHandle,
    ctx: egui::Context,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let server = FakeServer::start();
        let ctx = egui::Context::default();
        i18n::register(&ctx);
        icons::install_loaders(&ctx);
        ctx.enable_accesskit();
        let handle = OfflineHandle::start(
            OfflineStore::open(dir.path(), "lib1").unwrap(),
            Arc::new(FakeCatalog),
            server.url(),
            Token::new("t"),
            dir.path().join("offline/settings.toml"),
            ctx.clone(),
        );
        handle.set_online(Some(&library(1)));
        Fixture {
            _dir: dir,
            server,
            handle,
            ctx,
        }
    }

    /// One frame of `add_contents`; the texts on screen with their centres.
    fn frame(&self, add_contents: impl FnMut(&mut egui::Ui)) -> Vec<(String, egui::Pos2)> {
        self.frame_with(Vec::new(), add_contents)
    }

    fn frame_with(
        &self,
        events: Vec<egui::Event>,
        add_contents: impl FnMut(&mut egui::Ui),
    ) -> Vec<(String, egui::Pos2)> {
        let mut output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            add_contents,
        );
        output.textures_delta.clear();
        let mut texts = Vec::new();
        if let Some(update) = output.platform_output.accesskit_update {
            for (_, node) in update.nodes {
                let (Some(text), Some(b)) = (node.label().or(node.value()), node.bounds()) else {
                    continue;
                };
                let centre = egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32);
                texts.push((text.to_string(), centre));
            }
        }
        texts
    }

    /// Draws with `add_contents`, then clicks the widget whose text satisfies `is`, drawing
    /// again (the click lands on the second frame). Panics without such a widget.
    fn click(&self, is: impl Fn(&str) -> bool, mut add_contents: impl FnMut(&mut egui::Ui)) {
        let texts = self.frame(&mut add_contents);
        let (text, at) = texts
            .iter()
            .find(|(t, _)| is(t))
            .unwrap_or_else(|| panic!("nothing to click in {texts:?}"));
        eprintln!("click {text:?}");
        let press = |pressed| egui::Event::PointerButton {
            pos: *at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // Hover, press and release each take a frame to be seen by egui's input state.
        self.frame_with(vec![egui::Event::PointerMoved(*at)], &mut add_contents);
        self.frame_with(vec![press(true)], &mut add_contents);
        self.frame_with(vec![press(false)], &mut add_contents);
    }
}

fn has(texts: &[(String, egui::Pos2)], needle: &str) -> bool {
    texts.iter().any(|(t, _)| t.contains(needle))
}

fn saved(h: &OfflineHandle, id: &str) -> bool {
    h.status("set", id) == Some(PinStatus::Saved)
}

#[test]
fn the_toggle_shows_the_estimate_once_known() {
    let f = Fixture::new();
    f.server.set("s1", &["a", "b"]);
    let target = set_pin("s1");
    let texts = f.frame(|ui| {
        offline_toggle(ui, Some(&f.handle), &target);
    });
    assert!(has(&texts, "Save for offline"), "{texts:?}");
    f.handle.pin(target.clone());
    wait_for("the estimate", || {
        f.handle
            .pin_info("set", "s1")
            .is_some_and(|i| i.estimate.is_some())
    });
    let texts = f.frame(|ui| {
        offline_toggle(ui, Some(&f.handle), &target);
    });
    assert!(has(&texts, "MB"), "{texts:?}");
}

#[test]
fn the_toggle_pins_and_unpins() {
    let f = Fixture::new();
    f.server.set("s1", &["a"]);
    let target = set_pin("s1");
    let draw = |ui: &mut egui::Ui| {
        offline_toggle(ui, Some(&f.handle), &target);
    };
    f.click(|t| t == "Save for offline", draw);
    assert!(matches!(
        f.handle.status("set", "s1"),
        Some(PinStatus::Pending | PinStatus::Saved)
    ));
    wait_for("saved", || saved(&f.handle, "s1"));
    f.click(|t| t.starts_with("Saved for offline"), draw);
    assert_eq!(f.handle.status("set", "s1"), None);
}

#[test]
fn the_toggle_is_disabled_without_a_handle() {
    let f = Fixture::new();
    let target = set_pin("s1");
    let mut enabled = true;
    f.frame(|ui| {
        enabled = offline_toggle(ui, None, &target).enabled();
    });
    assert!(!enabled);
}

#[test]
fn the_mark_and_status_render() {
    let f = Fixture::new();
    f.frame(|ui| {
        saved_mark(ui);
        offline_status(ui, Some(&f.handle));
        offline_status(ui, None);
    });
}

fn two_pins(f: &Fixture) {
    f.server.set("s1", &["a"]);
    f.server.set("s2", &["b"]);
    for (id, name) in [("s1", "Trip"), ("s2", "Home")] {
        f.handle.pin(PinTarget {
            kind: "set".into(),
            id: id.into(),
            name: name.into(),
        });
    }
    wait_for("both saved", || {
        saved(&f.handle, "s1") && saved(&f.handle, "s2")
    });
}

#[test]
fn the_page_lists_pins_and_removes_one() {
    let f = Fixture::new();
    two_pins(&f);
    let mut page = OfflinePage::new();
    let texts = f.frame(|ui| page.ui(ui, Some(&f.handle)));
    assert!(has(&texts, "Trip") && has(&texts, "Home"), "{texts:?}");
    // Both rows have a "Remove": the first one is for the first pin.
    f.click(|t| t == "Remove", |ui| page.ui(ui, Some(&f.handle)));
    assert_eq!(f.handle.pins().len(), 1);
    let texts = f.frame(|ui| page.ui(ui, Some(&f.handle)));
    assert!(!has(&texts, "Trip") ^ !has(&texts, "Home"), "{texts:?}");
}

#[test]
fn the_page_without_a_handle_only_asks_to_connect() {
    let f = Fixture::new();
    let mut page = OfflinePage::new();
    let texts = f.frame(|ui| page.ui(ui, None));
    assert!(has(&texts, "Connect to a server"), "{texts:?}");
    assert!(!has(&texts, "Sync now"));
}

#[test]
fn the_limit_choice_is_saved() {
    let f = Fixture::new();
    assert!(LIMIT_CHOICES_GIB.contains(&16));
    let mut page = OfflinePage::new();
    f.click(|t| t == "16 GiB", |ui| page.ui(ui, Some(&f.handle)));
    assert_eq!(f.handle.limit_bytes(), 16 << 30);
}

#[test]
fn delete_all_asks_first() {
    let f = Fixture::new();
    two_pins(&f);
    let mut page = OfflinePage::new();
    f.click(|t| t == "Delete all", |ui| page.ui(ui, Some(&f.handle)));
    assert_eq!(f.handle.pins().len(), 2, "the first press only asks");
    // Cancelling keeps everything.
    f.click(|t| t == "Cancel", |ui| page.ui(ui, Some(&f.handle)));
    assert_eq!(f.handle.pins().len(), 2);
    f.click(|t| t == "Delete all", |ui| page.ui(ui, Some(&f.handle)));
    f.click(|t| t.starts_with("Yes"), |ui| page.ui(ui, Some(&f.handle)));
    wait_for("all deleted", || f.handle.pins().is_empty());
}
