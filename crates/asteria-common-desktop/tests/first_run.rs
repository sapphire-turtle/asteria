//! The first-run flow: its four pages, and how choosing another server skips the rest.

mod support;

use asteria_common_desktop::first_run_pages;
use corundum_framework::gui::first_run::FirstRun;
use support::{SPEC, click, context, frame, shows};

#[test]
fn there_are_four_pages() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        first_run_pages(&SPEC, dir.path().join("servers.toml")).len(),
        4
    );
}

/// Choose `answer` on the first page; the flow is then finished within a few frames,
/// having set nothing up.
fn finishes_after(answer: &str) {
    let dir = tempfile::tempdir().unwrap();
    let servers = dir.path().join("servers.toml");
    let ctx = context();
    let mut flow = FirstRun::new("Asteria Test", first_run_pages(&SPEC, servers.clone()));
    let mut finished = false;
    let mut setup_shown = false;
    let output = frame(&ctx, Vec::new(), |ui| {
        flow.ui(ui);
    });
    assert!(shows(&output, "Where is your library?"));
    click(&ctx, answer, |ui| finished |= flow.ui(ui));
    for _ in 0..10 {
        if finished {
            break;
        }
        let output = frame(&ctx, Vec::new(), |ui| finished |= flow.ui(ui));
        setup_shown |= shows(&output, "Choose a folder");
    }
    assert!(finished, "the flow did not finish after {answer:?}");
    assert!(FirstRun::done(&ctx));
    assert!(!setup_shown, "the setup page was drawn");
    assert!(!servers.exists());
}

#[test]
fn another_server_skips_the_rest() {
    finishes_after("Another server");
}

#[test]
fn later_skips_the_rest() {
    finishes_after("Later");
}

#[test]
fn this_computer_goes_on_to_the_setup() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let mut flow = FirstRun::new(
        "Asteria Test",
        first_run_pages(&SPEC, dir.path().join("servers.toml")),
    );
    click(&ctx, "This computer", |ui| {
        flow.ui(ui);
    });
    let output = frame(&ctx, Vec::new(), |ui| {
        flow.ui(ui);
    });
    assert!(shows(&output, "Choose a folder"));
    assert!(!FirstRun::done(&ctx));
}
