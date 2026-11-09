//! [`OfflinePage`]: how much space the offline copies take, what is saved, and the controls.

use corundum_framework::egui;
use corundum_framework::gui::tr;

use super::{LIMIT_CHOICES_GIB, OfflineHandle, SyncPhase};
use crate::widgets::offline_toggle::{size_text, state_text};

/// The "Offline" settings page. It keeps only what the user is in the middle of (the
/// confirmation of "Delete all"); everything else is read from the handle every frame.
#[derive(Default)]
pub struct OfflinePage {
    confirm_clear: bool,
}

impl OfflinePage {
    pub fn new() -> OfflinePage {
        OfflinePage::default()
    }

    /// `handle` is `None` before the app has a library to keep offline.
    pub fn ui(&mut self, ui: &mut egui::Ui, handle: Option<&OfflineHandle>) {
        let Some(handle) = handle else {
            self.confirm_clear = false;
            ui.weak(tr!(ui, "asteria-common-client-offline-needs-server"));
            return;
        };
        let progress = handle.progress();
        let used = size_text(progress.used_bytes);
        let limit = size_text(progress.limit_bytes);
        ui.label(tr!(
            ui,
            "asteria-common-client-offline-usage",
            used = used.as_str(),
            limit = limit.as_str()
        ));
        let fraction = progress.used_bytes as f32 / progress.limit_bytes.max(1) as f32;
        ui.add(egui::ProgressBar::new(fraction.min(1.0)));
        ui.horizontal_wrapped(|ui| {
            ui.label(tr!(ui, "asteria-common-client-offline-limit"));
            for gib in LIMIT_CHOICES_GIB {
                let chosen = progress.limit_bytes == gib << 30;
                if ui.selectable_label(chosen, format!("{gib} GiB")).clicked() && !chosen {
                    handle.set_limit_bytes(gib << 30);
                }
            }
        });
        match &progress.phase {
            SyncPhase::Failed(reason) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tr!(
                        ui,
                        "asteria-common-client-offline-failed",
                        reason = reason.as_str()
                    ),
                );
            }
            SyncPhase::Full => {
                ui.label(tr!(ui, "asteria-common-client-offline-status-full"));
            }
            SyncPhase::Syncing { done, total } => {
                ui.label(tr!(
                    ui,
                    "asteria-common-client-offline-status-syncing",
                    done = *done,
                    total = *total
                ));
            }
            SyncPhase::Idle => {}
        }
        if progress.unsavable > 0 {
            ui.weak(tr!(
                ui,
                "asteria-common-client-offline-unsavable",
                count = progress.unsavable
            ));
        }
        ui.separator();
        let pins = handle.pins();
        if pins.is_empty() {
            ui.weak(tr!(ui, "asteria-common-client-offline-empty"));
        }
        for info in &pins {
            let target = &info.pin.target;
            ui.horizontal_wrapped(|ui| {
                ui.strong(&target.name);
                ui.weak(size_text(info.bytes));
                ui.label(state_text(ui, info.pin.status, &progress.phase));
                if ui
                    .button(tr!(ui, "asteria-common-client-offline-remove"))
                    .clicked()
                {
                    handle.unpin(&target.kind, &target.id);
                }
            });
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if self.confirm_clear {
                ui.label(tr!(ui, "asteria-common-client-offline-clear-confirm"));
                if ui
                    .button(tr!(ui, "asteria-common-client-offline-clear-yes"))
                    .clicked()
                {
                    handle.clear_all();
                    self.confirm_clear = false;
                }
                if ui
                    .button(tr!(ui, "asteria-common-client-offline-cancel"))
                    .clicked()
                {
                    self.confirm_clear = false;
                }
                return;
            }
            if ui
                .button(tr!(ui, "asteria-common-client-offline-sync-now"))
                .clicked()
            {
                handle.sync_now();
            }
            if ui
                .button(tr!(ui, "asteria-common-client-offline-clear"))
                .clicked()
            {
                self.confirm_clear = true;
            }
        });
    }
}
