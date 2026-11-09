//! Display settings, kept in `display.toml`, and their Settings page.
//!
//! The file is read once at start (a missing or unreadable one gives the defaults) and
//! written by a background thread whenever a setting changes.

use std::io::Write;
use std::path::Path;

use asteria_common_client::images::Preset;
use asteria_common_client::session::Session;
use corundum_framework::egui;
use corundum_framework::gui::settings::SettingsSection;
use corundum_framework::gui::tr;
use serde::{Deserialize, Serialize};

/// What the user chose about showing pictures.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplaySettings {
    /// Show the smaller `offline-image` rather than `preview` (the saved copy when there is one).
    pub low_quality: bool,
}

impl DisplaySettings {
    /// A missing or unreadable file gives the defaults (logged).
    pub fn load(path: &Path) -> DisplaySettings {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return DisplaySettings::default();
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "reading display.toml failed");
                return DisplaySettings::default();
            }
        };
        match toml::from_str::<DisplaySettings>(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "display.toml is not valid");
                DisplaySettings::default()
            }
        }
    }

    /// Writes to a temporary file beside `path`, then renames it over `path`.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        let result = (|| {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// The preset the viewer and the detail pane draw: `offline-image` when the setting asks for
/// it or the session is offline (only that one is saved), `preview` otherwise.
pub(crate) fn display_preset(session: &Session, settings: &DisplaySettings) -> Preset {
    if settings.low_quality || session.is_offline() {
        Preset::OfflineImage
    } else {
        Preset::Preview
    }
}

/// The "Display" page of Settings: holds the settings as the user changes them.
pub struct DisplayPage {
    pub settings: DisplaySettings,
    changed: bool,
    error: Option<String>,
}

impl DisplayPage {
    pub fn new(settings: DisplaySettings) -> DisplayPage {
        DisplayPage {
            settings,
            changed: false,
            error: None,
        }
    }

    /// The settings, if the user changed them since the last call.
    pub fn take_change(&mut self) -> Option<DisplaySettings> {
        std::mem::take(&mut self.changed).then(|| self.settings.clone())
    }

    pub fn show_error(&mut self, why: String) {
        self.error = Some(why);
    }
}

impl SettingsSection for DisplayPage {
    fn label(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-photos-display-title")
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(tr!(ui, "asteria-photos-display-title"));
        ui.separator();
        if let Some(why) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, why);
        }
        let mut low = self.settings.low_quality;
        if ui
            .checkbox(&mut low, tr!(ui, "asteria-photos-display-low-quality"))
            .changed()
        {
            self.settings.low_quality = low;
            self.changed = true;
        }
        ui.weak(tr!(ui, "asteria-photos-display-low-quality-hint"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("display.toml");
        assert_eq!(DisplaySettings::load(&path), DisplaySettings::default());
        assert!(!DisplaySettings::load(&path).low_quality);
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/display.toml");
        let s = DisplaySettings { low_quality: true };
        s.save(&path).unwrap();
        assert_eq!(DisplaySettings::load(&path), s);
        // A key it does not know, or a broken file, gives the defaults for what is missing.
        std::fs::write(&path, "other = 1\n").unwrap();
        assert_eq!(DisplaySettings::load(&path), DisplaySettings::default());
        std::fs::write(&path, "low_quality = \"yes\"\n").unwrap();
        assert_eq!(DisplaySettings::load(&path), DisplaySettings::default());
    }
}
