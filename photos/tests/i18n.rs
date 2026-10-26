//! The app's translation files: they parse, both languages have the same keys, and every id
//! the code uses exists.

use asteria_photos::i18n::RESOURCES;
use corundum_framework::gui::i18n::check;

#[test]
fn files_are_consistent() {
    assert!(check::parse_errors(&RESOURCES).is_empty());
    assert!(check::key_difference(&RESOURCES, "en", "ja").is_empty());
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    assert_eq!(
        check::unknown_ids(&RESOURCES, "en", &src, "asteria-photos-", &[]),
        Vec::<String>::new()
    );
}
