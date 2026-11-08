//! The save-for-offline toggle every app puts on what can be kept offline, the small mark for
//! lists, and the status bar's text about the sync.

use corundum_framework::egui;
use corundum_framework::gui::theme::{Swatch, Tone};
use corundum_framework::gui::{icons, tr};

use crate::offline::{OfflineHandle, PinStatus, PinTarget, SyncPhase};

/// How much space a file of `bytes` takes, for people: whole MB, or GB with a decimal from 1 GB.
pub fn size_text(bytes: u64) -> String {
    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{} MB", bytes.div_ceil(MB).max(1))
    }
}

/// What a pin is doing, as a sentence. A pin still waiting for its answers is "saving",
/// unless the size limit stopped the sync: then it is "full".
pub fn state_text(ui: &egui::Ui, status: PinStatus, phase: &SyncPhase) -> String {
    match (status, phase) {
        (PinStatus::Saved, _) => tr!(ui, "asteria-common-client-offline-saved"),
        (PinStatus::Gone, _) => tr!(ui, "asteria-common-client-offline-gone"),
        (PinStatus::Pending, SyncPhase::Full) => tr!(ui, "asteria-common-client-offline-full"),
        (PinStatus::Pending, _) => tr!(ui, "asteria-common-client-offline-syncing"),
    }
}

/// "Save for offline" for what `target` names; once saved, its state (and the estimated
/// size, when the server has answered) and a click takes it away again. Disabled while there
/// is no `handle` (no data directory, or not connected yet).
pub fn offline_toggle(
    ui: &mut egui::Ui,
    handle: Option<&OfflineHandle>,
    target: &PinTarget,
) -> egui::Response {
    let Some(handle) = handle else {
        return ui.add_enabled(
            false,
            egui::Button::new(tr!(ui, "asteria-common-client-offline-save")),
        );
    };
    let Some(info) = handle.pin_info(&target.kind, &target.id) else {
        let response = ui.button(tr!(ui, "asteria-common-client-offline-save"));
        if response.clicked() {
            handle.pin(target.clone());
        }
        return response;
    };
    let mut text = state_text(ui, info.pin.status, &handle.progress().phase);
    if let Some(estimate) = info.estimate {
        let size = size_text(estimate);
        text.push_str(&tr!(
            ui,
            "asteria-common-client-offline-estimate",
            size = size.as_str()
        ));
    }
    let response = ui
        .button(text)
        .on_hover_text(tr!(ui, "asteria-common-client-offline-remove"));
    if response.clicked() {
        handle.unpin(&target.kind, &target.id);
    }
    response
}

/// The small mark a list shows on what is saved for offline.
pub fn saved_mark(ui: &mut egui::Ui) {
    let image = icons::image_as(ui, icons::download(), 14.0, Tone::Text, Swatch::Neutral);
    ui.add(image)
        .on_hover_text(tr!(ui, "asteria-common-client-offline-saved"));
}

/// The status bar's text about the sync: the progress while saving, and why it stopped when it
/// did. Nothing otherwise.
pub fn offline_status(ui: &mut egui::Ui, handle: Option<&OfflineHandle>) {
    let Some(handle) = handle else {
        return;
    };
    match handle.progress().phase {
        SyncPhase::Syncing { done, total } => {
            ui.label(tr!(
                ui,
                "asteria-common-client-offline-status-syncing",
                done = done,
                total = total
            ));
        }
        SyncPhase::Full => {
            ui.label(tr!(ui, "asteria-common-client-offline-status-full"));
        }
        SyncPhase::Failed(_) => {
            ui.label(tr!(ui, "asteria-common-client-offline-status-failed"));
        }
        SyncPhase::Idle => {}
    }
}
