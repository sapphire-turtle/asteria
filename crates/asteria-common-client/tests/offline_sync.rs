//! The offline sync against a stub server: answers and media are saved, failures are counted,
//! and nothing is deleted that a pin still needs.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use asteria_common_api::ItemId;
use asteria_common_client::offline::{
    DEFAULT_LIMIT_BYTES, OfflineHandle, OfflineStore, PinStatus, SyncPhase,
};
use asteria_common_client::profiles::Token;
use corundum_framework::egui;

use common::offline::{
    ESTIMATE, FakeCatalog, FakeServer, SetAnswer, file_hash, library, set_pin, wait_for,
};

/// A temporary data directory and a stub server.
struct Fixture {
    dir: tempfile::TempDir,
    server: FakeServer,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            dir: tempfile::tempdir().unwrap(),
            server: FakeServer::start(),
        }
    }

    fn settings(&self) -> PathBuf {
        self.dir.path().join("offline/settings.toml")
    }

    /// A handle on the library `lib1`, offline until `set_online` is called.
    fn handle(&self) -> OfflineHandle {
        OfflineHandle::start(
            OfflineStore::open(self.dir.path(), "lib1").unwrap(),
            Arc::new(FakeCatalog),
            self.server.url(),
            Token::new("t"),
            self.settings(),
            egui::Context::default(),
        )
    }
}

fn has(handle: &OfflineHandle, item: &str) -> bool {
    handle.media(&file_hash(item), "thumb").is_some()
}

fn saved(handle: &OfflineHandle, set: &str) -> bool {
    handle.status("set", set) == Some(PinStatus::Saved)
}

fn idle(handle: &OfflineHandle) -> bool {
    handle.progress().phase == SyncPhase::Idle
}

/// Pins `set` online and waits until it and its media are saved.
fn pin_and_wait(handle: &OfflineHandle, set: &str, items: &[&str]) {
    handle.pin(set_pin(set));
    wait_for(set, || {
        saved(handle, set) && idle(handle) && items.iter().all(|i| has(handle, i))
    });
}

/// Long enough for a request that should not come to have come.
fn settle() {
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
fn pinning_saves_docs_and_media() {
    let f = Fixture::new();
    f.server.set("s1", &["a", "b"]);
    let h = f.handle();
    h.set_online(Some(&library(1)));
    pin_and_wait(&h, "s1", &["a", "b"]);
    assert_eq!(
        h.media_of_item(&ItemId("a".into()), "thumb"),
        h.media(&file_hash("a"), "thumb")
    );
    assert!(h.docs().get("set", "s1").is_some());
    assert_eq!(h.progress().used_bytes, 2 * ESTIMATE);
    assert_eq!(f.server.requests("GET /media/a/thumb"), 1);
}

#[test]
fn media_that_cannot_be_rendered_is_counted() {
    let f = Fixture::new();
    f.server.set("s1", &["a", "b"]);
    f.server.media_status("a", 422);
    let h = f.handle();
    h.set_online(Some(&library(1)));
    pin_and_wait(&h, "s1", &["b"]);
    assert_eq!(h.progress().unsavable, 1);
    assert!(!has(&h, "a"));
}

#[test]
fn a_pin_the_server_no_longer_has_is_gone() {
    let f = Fixture::new();
    f.server.set("s1", &["a", "b"]);
    let h = f.handle();
    h.set_online(Some(&library(1)));
    pin_and_wait(&h, "s1", &["a", "b"]);
    f.server.answer("s1", SetAnswer::NotFound);
    h.sync_now();
    wait_for("gone", || {
        h.status("set", "s1") == Some(PinStatus::Gone) && idle(&h)
    });
    assert!(h.docs().get("set", "s1").is_some());
    assert!(has(&h, "a") && has(&h, "b"));
}

#[test]
fn a_sync_cut_off_midway_deletes_nothing() {
    let f = Fixture::new();
    f.server.set("s1", &["a", "b"]);
    f.server.set("s2", &["c"]);
    let h = f.handle();
    h.set_online(Some(&library(1)));
    pin_and_wait(&h, "s1", &["a", "b"]);
    pin_and_wait(&h, "s2", &["c"]);
    // `b` is no longer in `s1`, but the sync that learns it cannot fetch `s2`.
    f.server.set("s1", &["a"]);
    f.server.answer("s2", SetAnswer::HangUp);
    h.sync_now();
    wait_for("failed", || {
        matches!(h.progress().phase, SyncPhase::Failed(_))
    });
    assert!(has(&h, "a") && has(&h, "b") && has(&h, "c"));
    assert!(h.docs().get("set", "s2").is_some());

    f.server.set("s2", &["c"]);
    h.sync_now();
    wait_for("both saved", || {
        saved(&h, "s1") && saved(&h, "s2") && idle(&h)
    });
    // Now that every pin answered, what nothing points to is cleaned up.
    assert!(has(&h, "a") && has(&h, "c"));
    assert!(!has(&h, "b"));
}

#[test]
fn unpinning_keeps_media_another_pin_uses() {
    let f = Fixture::new();
    f.server.set("s1", &["a", "shared"]);
    f.server.set("s2", &["b", "shared"]);
    let h = f.handle();
    h.set_online(Some(&library(1)));
    pin_and_wait(&h, "s1", &["a", "shared"]);
    pin_and_wait(&h, "s2", &["b", "shared"]);
    h.unpin("set", "s1");
    assert_eq!(h.status("set", "s1"), None);
    wait_for("a removed", || !has(&h, "a"));
    assert!(has(&h, "shared") && has(&h, "b"));
    assert!(h.docs().get("set", "s1").is_none());
    assert!(h.docs().get("set", "s2").is_some());
    assert_eq!(h.pins().len(), 1);
}

#[test]
fn a_lower_limit_stops_downloads_but_keeps_files() {
    let f = Fixture::new();
    f.server.set("s1", &["a"]);
    f.server.set("s2", &["b"]);
    let h = f.handle();
    h.set_online(Some(&library(1)));
    pin_and_wait(&h, "s1", &["a"]);
    h.set_limit_bytes(1);
    h.pin(set_pin("s2"));
    wait_for("full", || h.progress().phase == SyncPhase::Full);
    assert!(has(&h, "a"));
    assert!(!has(&h, "b"));
    assert_eq!(f.server.requests("GET /media/b/"), 0);
    assert_eq!(h.progress().limit_bytes, 1);
}

#[test]
fn pin_info_reports_the_estimate() {
    let f = Fixture::new();
    f.server.set("s1", &["a", "b"]);
    let h = f.handle();
    h.pin(set_pin("s1"));
    let info = h.pin_info("set", "s1").unwrap();
    assert_eq!((info.estimate, info.bytes), (None, 0));
    assert_eq!(info.pin.status, PinStatus::Pending);
    h.set_online(Some(&library(1)));
    wait_for("saved", || {
        saved(&h, "s1") && idle(&h) && has(&h, "a") && has(&h, "b")
    });
    let info = h.pin_info("set", "s1").unwrap();
    assert_eq!(info.estimate, Some(2 * ESTIMATE));
    assert_eq!(info.bytes, 2 * ESTIMATE);
    assert_eq!(h.pins(), vec![info]);
}

#[test]
fn the_limit_is_kept_across_handles() {
    let f = Fixture::new();
    let h = f.handle();
    assert_eq!(h.limit_bytes(), DEFAULT_LIMIT_BYTES);
    h.set_limit_bytes(2 << 30);
    assert_eq!(h.limit_bytes(), 2 << 30);
    // Dropping the handle waits for the setting to be written, as closing the app does.
    drop(h);
    assert_eq!(f.handle().limit_bytes(), 2 << 30);
}

#[test]
fn offline_pins_wait_for_the_server() {
    let f = Fixture::new();
    f.server.set("s1", &["a"]);
    let h = f.handle();
    h.set_online(None);
    h.pin(set_pin("s1"));
    h.sync_now();
    settle();
    assert_eq!(f.server.requests(""), 0);
    assert_eq!(h.status("set", "s1"), Some(PinStatus::Pending));
    h.set_online(Some(&library(1)));
    wait_for("saved", || saved(&h, "s1") && idle(&h) && has(&h, "a"));
}

#[test]
fn an_unchanged_mark_does_not_sync_again() {
    let f = Fixture::new();
    f.server.set("s1", &["a"]);
    let h = f.handle();
    h.pin(set_pin("s1"));
    h.set_online(Some(&library(1)));
    wait_for("saved", || saved(&h, "s1") && idle(&h) && has(&h, "a"));
    h.set_online(Some(&library(1)));
    settle();
    assert_eq!(f.server.requests("set.get"), 1);
    h.set_online(Some(&library(2)));
    wait_for("second sync", || f.server.requests("set.get") == 2);
}

#[test]
fn pins_survive_a_new_handle() {
    let f = Fixture::new();
    f.server.set("s1", &["a"]);
    let h = f.handle();
    h.set_online(Some(&library(1)));
    pin_and_wait(&h, "s1", &["a"]);
    drop(h);
    let h = f.handle();
    assert_eq!(h.status("set", "s1"), Some(PinStatus::Saved));
    wait_for("docs loaded", || h.docs().get("set", "s1").is_some());
    assert!(has(&h, "a"));
    h.unpin("set", "s1");
    wait_for("a removed", || !has(&h, "a"));
    assert!(h.docs().get("set", "s1").is_none());
}
