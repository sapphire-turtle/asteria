//! Playback settings, kept in `playback.toml`, and their Settings page.
//!
//! The file is read once at start (a missing or unreadable one gives the defaults) and
//! written by a background thread whenever a setting changes.

use std::io::Write;
use std::path::Path;

use corundum_framework::egui;
use corundum_framework::gui::settings::SettingsSection;
use corundum_framework::gui::tr;
use serde::{Deserialize, Serialize};

/// What the user chose about playback.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackSettings {
    /// Stream the server's offline rendition (smaller) rather than the original file.
    pub low_quality: bool,
    /// 0.0..=1.0.
    pub volume: f32,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        PlaybackSettings {
            low_quality: false,
            volume: 1.0,
        }
    }
}

impl PlaybackSettings {
    /// A missing or unreadable file gives the defaults (logged); volume is clamped.
    pub fn load(path: &Path) -> PlaybackSettings {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return PlaybackSettings::default();
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "reading playback.toml failed");
                return PlaybackSettings::default();
            }
        };
        match toml::from_str::<PlaybackSettings>(&text) {
            Ok(s) => s.clamped(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "playback.toml is not valid");
                PlaybackSettings::default()
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

    fn clamped(mut self) -> PlaybackSettings {
        self.volume = if self.volume.is_nan() {
            1.0
        } else {
            self.volume.clamp(0.0, 1.0)
        };
        self
    }
}

/// The "Playback" page of Settings: holds the settings as the user changes them.
pub(crate) struct PlaybackPage {
    pub settings: PlaybackSettings,
    changed: bool,
    error: Option<String>,
}

impl PlaybackPage {
    pub fn new(settings: PlaybackSettings) -> PlaybackPage {
        PlaybackPage {
            settings,
            changed: false,
            error: None,
        }
    }

    /// The settings, if the user changed them since the last call.
    pub fn take_change(&mut self) -> Option<PlaybackSettings> {
        std::mem::take(&mut self.changed).then(|| self.settings.clone())
    }

    pub fn show_error(&mut self, why: String) {
        self.error = Some(why);
    }
}

impl SettingsSection for PlaybackPage {
    fn label(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-music-playback-title")
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(tr!(ui, "asteria-music-playback-title"));
        ui.separator();
        if let Some(why) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, why);
        }
        let mut low = self.settings.low_quality;
        if ui
            .checkbox(&mut low, tr!(ui, "asteria-music-playback-low-quality"))
            .changed()
        {
            self.settings.low_quality = low;
            self.changed = true;
        }
        ui.weak(tr!(ui, "asteria-music-playback-low-quality-hint"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/playback.toml");
        let s = PlaybackSettings {
            low_quality: true,
            volume: 0.4,
        };
        s.save(&path).unwrap();
        assert_eq!(PlaybackSettings::load(&path), s);
        std::fs::write(&path, "volume = 7.0\n").unwrap();
        assert_eq!(PlaybackSettings::load(&path).volume, 1.0);
        assert!(!PlaybackSettings::load(&path).low_quality);
    }
}
