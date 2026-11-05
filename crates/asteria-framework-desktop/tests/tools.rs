//! The external tools section shows the tools of the app, and says so when it needs none.

mod support;

use asteria_framework_desktop::tools_ui;
use asteria_framework_server::Tool;
use support::{context, frame, shows};

#[test]
fn only_the_apps_tools_are_listed() {
    let ctx = context();
    let output = frame(&ctx, Vec::new(), |ui| tools_ui(ui, &[Tool::Vipsthumbnail]));
    assert!(shows(&output, "vipsthumbnail"));
    assert!(shows(
        &output,
        "Thumbnails of HEIC, AVIF and some camera RAW files."
    ));
    assert!(!shows(&output, "ffmpeg"));
    let output = frame(&ctx, Vec::new(), |ui| {
        tools_ui(ui, &[Tool::Vipsthumbnail, Tool::Ffmpeg])
    });
    assert!(shows(&output, "ffmpeg"));
    assert!(shows(&output, "Video thumbnails."));
    assert!(!shows(&output, "offline"));
}

#[test]
fn an_app_without_tools_says_it_needs_none() {
    let ctx = context();
    let output = frame(&ctx, Vec::new(), |ui| tools_ui(ui, &[]));
    assert!(shows(&output, "needs no external tools"));
    assert!(!shows(&output, "Check again"));
}

#[test]
fn the_spec_names_its_tools() {
    assert_eq!(support::SPEC.tools, [Tool::Vipsthumbnail]);
}
