//! Settings › About (spec §5.9): the app's name and version, its update row, its command
//! line on `PATH` (macOS and Linux), its entry in the app menu, and the licences.

use asteria_common_client::settings::ExtraSettingsPage;
use asteria_common_server::Tool;
use corundum_framework::egui;
use corundum_framework::gui::settings::AboutRows;
use corundum_framework::gui::tr;
use corundum_framework::gui::update::{AfterUpdate, Target, UpdateTarget, UpdatesPanel, semver};
use corundum_framework::package::CliSpec;
use corundum_framework::update::ReleaseSource;

/// The About page of a desktop app: corundum's [`AboutRows`] (its update row is made when
/// the page is first drawn, so nothing looks for a release before the page is opened)
/// between the app's name and version and its licences.
pub struct AboutPage {
    /// What the page and the update row call the app: `Asteria Photos`.
    label: &'static str,
    /// The update row, the command line's (not on Windows, where the install hook puts the
    /// command on `PATH`) and the app menu's.
    rows: AboutRows,
    /// The licence line: which tools the app's packages carry.
    licence: &'static str,
}

impl AboutPage {
    /// The page for the app called `label`, whose package puts `cli` on `PATH`, in the window
    /// `ctx` belongs to: its app-menu row is the spec the window was given
    /// (`WindowOptions::launcher`). `tools` are the external tools its server uses (its
    /// [`LocalServerSpec::tools`](crate::LocalServerSpec)), which name what the licence line
    /// says its packages carry.
    pub fn new(
        label: &'static str,
        cli: CliSpec,
        ctx: &egui::Context,
        tools: &[Tool],
    ) -> AboutPage {
        let rows = AboutRows::for_window(ctx).updates_later(move || AboutPage::updates(label));
        #[cfg(windows)]
        let _ = cli;
        #[cfg(not(windows))]
        let rows = rows.cli(cli);
        AboutPage {
            label,
            rows,
            licence: licence_id(tools),
        }
    }

    /// The update row: the package's when the desktop runs from one, else the release the
    /// executable came from.
    fn updates(label: &str) -> UpdatesPanel {
        let binary = UpdateTarget {
            label: label.to_owned(),
            current: semver::Version::parse(env!("CARGO_PKG_VERSION")).ok(),
            source: ReleaseSource::github("sapphire-turtle", "asteria"),
            // The source names no asset, so nothing is ever replaced: the row shows the
            // newest release and where to get it.
            exe: std::env::current_exe().unwrap_or_default(),
            after: AfterUpdate::Relaunch,
        };
        let target = Target::this_app(label.to_owned(), crate::updater(), binary);
        UpdatesPanel::new(vec![target], "asteria")
    }
}

impl ExtraSettingsPage for AboutPage {
    fn label(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-common-desktop-about")
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(self.label);
        ui.label(tr!(
            ui,
            "asteria-common-desktop-about-version",
            version = env!("CARGO_PKG_VERSION")
        ));
        ui.add_space(8.0);
        self.rows.ui(ui);
        ui.add_space(16.0);
        ui.label(egui::RichText::new(tr!(ui, self.licence)).weak());
    }
}

/// The licence line for an app whose server uses `tools`. The packages carry ffmpeg on
/// Windows and macOS when the app uses it, and libvips on Windows when it uses
/// vipsthumbnail (macOS has sips); see packaging/fetch-tools.sh.
fn licence_id(tools: &[Tool]) -> &'static str {
    match (
        tools.contains(&Tool::Ffmpeg),
        tools.contains(&Tool::Vipsthumbnail),
    ) {
        (true, _) => "asteria-common-desktop-about-licence-ffmpeg-vips",
        (false, true) => "asteria-common-desktop-about-licence-vips",
        (false, false) => "asteria-common-desktop-about-licence-none",
    }
}

#[cfg(test)]
mod tests {
    use corundum_framework::gui::i18n::{self, Language, LanguageSettings};

    use super::*;

    const IDS: [&str; 5] = [
        "asteria-common-desktop-about",
        "asteria-common-desktop-about-version",
        "asteria-common-desktop-about-licence-ffmpeg-vips",
        "asteria-common-desktop-about-licence-vips",
        "asteria-common-desktop-about-licence-none",
    ];

    /// A context with this crate's strings in `lang`.
    fn ctx_in(lang: &str) -> egui::Context {
        let ctx = egui::Context::default();
        crate::register(&ctx);
        i18n::install(
            &ctx,
            &LanguageSettings {
                language: Language::Explicit(lang.to_owned()),
            },
        );
        ctx
    }

    #[test]
    fn every_id_resolves_in_english_and_japanese() {
        let en = ctx_in("en");
        let ja = ctx_in("ja");
        for id in IDS {
            let args = [("version", i18n::Arg::from("0.1.0"))];
            let in_en = i18n::tr_args(&en, id, &args);
            let in_ja = i18n::tr_args(&ja, id, &args);
            assert_ne!(in_en, id, "{id} has no English");
            assert_ne!(in_ja, id, "{id} has no Japanese");
            assert_ne!(in_en, in_ja, "{id} is not translated");
        }
        assert!(
            i18n::tr_args(&en, IDS[1], &[("version", i18n::Arg::from("0.1.0"))]).contains("0.1.0")
        );
    }

    #[test]
    fn the_licence_names_only_what_the_app_bundles() {
        assert_eq!(
            licence_id(&[Tool::Vipsthumbnail, Tool::Ffmpeg]),
            "asteria-common-desktop-about-licence-ffmpeg-vips"
        );
        assert_eq!(
            licence_id(&[Tool::Vipsthumbnail]),
            "asteria-common-desktop-about-licence-vips"
        );
        assert_eq!(licence_id(&[]), "asteria-common-desktop-about-licence-none");
        for lang in ["en", "ja"] {
            let ctx = ctx_in(lang);
            let music = i18n::tr_args(&ctx, licence_id(&[Tool::Vipsthumbnail]), &[]);
            assert!(!music.contains("ffmpeg"), "{lang}: {music}");
            assert!(music.contains("libvips"), "{lang}: {music}");
            let photos = i18n::tr_args(&ctx, licence_id(&[Tool::Vipsthumbnail, Tool::Ffmpeg]), &[]);
            assert!(
                photos.contains("ffmpeg") && photos.contains("libvips"),
                "{lang}: {photos}"
            );
            let none = i18n::tr_args(&ctx, licence_id(&[]), &[]);
            assert!(
                !none.contains("ffmpeg") && !none.contains("libvips"),
                "{lang}: {none}"
            );
        }
    }
}
