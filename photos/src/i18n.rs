//! The app's strings, in English and Japanese, for corundum's `tr!`.

use corundum_framework::egui;
use corundum_framework::gui::i18n::{self, Resources};

/// The Fluent files, embedded. Ids are `asteria-photos-<area>-<what>`.
pub const RESOURCES: Resources = Resources {
    name: "asteria-photos",
    files: &[
        ("en", include_str!("../i18n/en/photos.ftl")),
        ("ja", include_str!("../i18n/ja/photos.ftl")),
    ],
};

/// Add the app's strings, and those of the client crate under it, to `ctx`'s translations.
pub fn register(ctx: &egui::Context) {
    asteria_framework_client::i18n::register(ctx);
    i18n::register(ctx, RESOURCES);
}
