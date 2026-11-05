//! This crate's strings, in English and Japanese, for corundum's `tr!`.
//!
//! An app calls [`register`] once with its egui context (it is cheap to call again).

use corundum_framework::egui;
use corundum_framework::gui::i18n::{self, Resources};

/// The Fluent files, embedded. Ids are `asteria-common-desktop-<area>-<what>`.
pub const RESOURCES: Resources = Resources {
    name: "asteria-common-desktop",
    files: &[
        ("en", include_str!("../i18n/en/desktop.ftl")),
        ("ja", include_str!("../i18n/ja/desktop.ftl")),
    ],
};

/// Add this crate's strings to `ctx`'s translations.
pub fn register(ctx: &egui::Context) {
    i18n::register(ctx, RESOURCES);
}
