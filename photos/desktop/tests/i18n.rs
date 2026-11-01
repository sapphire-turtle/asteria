//! The strings the desktop window uses itself (the drop overlay) exist in the app's
//! translation files, which live in `asteria-photos-gui`.

use asteria_photos_gui::i18n::RESOURCES;
use corundum_framework::gui::i18n::check;

#[test]
fn every_id_the_desktop_uses_exists() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for lang in ["en", "ja"] {
        assert_eq!(
            check::unknown_ids(&RESOURCES, lang, &src, "asteria-photos-", &[]),
            Vec::<String>::new()
        );
    }
}
