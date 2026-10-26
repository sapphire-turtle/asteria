//! The Uploads screen: what was dropped on the app and where each group of files is on its way
//! to the server's `Inbox/`, and the little the status bar says about it.
//!
//! The list is the engine's snapshot, taken every frame (it is only an `Arc` clone); only the
//! rows in view are drawn, so thousands of groups cost no more than a screenful.

use asteria_framework_client::errors::error_text;
use asteria_framework_client::upload::queue::{GroupId, GroupState, QueueSummary, QueuedGroup};
use asteria_framework_client::upload::{PlanWarning, UploadSnapshot, Uploads, UploadsState};
use corundum_framework::egui;
use corundum_framework::gui::{icons, tr};

/// What the user may do with a group in a given state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Actions {
    /// "Save": send it although the library has it (a group waiting for a decision).
    pub keep: bool,
    /// "Discard": do not send it (the same).
    pub discard: bool,
    /// "Try again" (a failed group).
    pub retry: bool,
    /// "Cancel" (any group not finished).
    pub cancel: bool,
}

/// The buttons a row in `state` has.
pub fn actions(state: &GroupState) -> Actions {
    match state {
        GroupState::AwaitingDecision { .. } => Actions {
            keep: true,
            discard: true,
            cancel: true,
            retry: false,
        },
        GroupState::Failed { .. } => Actions {
            retry: true,
            cancel: true,
            ..Actions::default()
        },
        GroupState::Done { .. } | GroupState::Discarded => Actions::default(),
        _ => Actions {
            cancel: true,
            ..Actions::default()
        },
    }
}

/// What the status bar shows while some group is unfinished (not done, not discarded):
/// `(done, total, waiting for a decision)`, from the queue's own [`QueueSummary`], so "12 / 40"
/// is groups done over all groups in the queue. `None` when everything is finished.
pub fn status_counts(
    groups: &[QueuedGroup],
    summary: &QueueSummary,
) -> Option<(usize, usize, usize)> {
    let unfinished = groups
        .iter()
        .any(|g| !matches!(g.state, GroupState::Done { .. } | GroupState::Discarded));
    unfinished.then_some((summary.done, summary.total, summary.awaiting))
}

/// What the screen remembers between frames.
#[derive(Debug, Default)]
pub(crate) struct UploadsView {
    /// The waiting group whose existing files are shown.
    opened: Option<GroupId>,
}

/// The Uploads screen for `uploads` (`None`: not connected, or the app has no data directory).
pub(crate) fn ui(ui: &mut egui::Ui, uploads: Option<&Uploads>, view: &mut UploadsView) {
    egui::CentralPanel::default().show(ui, |ui| {
        let Some(uploads) = uploads else {
            ui.weak(tr!(ui, "asteria-photos-uploads-unavailable"));
            return;
        };
        let snap = uploads.snapshot();
        header(ui, uploads, &snap);
        notes(ui, &snap);
        if snap.groups.is_empty() {
            ui.weak(tr!(ui, "asteria-photos-uploads-empty"));
            return;
        }
        if let Some(open) = view.opened {
            match snap.groups.iter().find(|g| g.id == open) {
                Some(QueuedGroup {
                    state: GroupState::AwaitingDecision { existing },
                    ..
                }) => existing_ui(ui, existing),
                _ => view.opened = None,
            }
        }
        list(ui, uploads, &snap, view);
    });
}

fn header(ui: &mut egui::Ui, uploads: &Uploads, snap: &UploadSnapshot) {
    ui.horizontal(|ui| {
        let finished = snap
            .groups
            .iter()
            .any(|g| matches!(g.state, GroupState::Done { .. } | GroupState::Discarded));
        if ui
            .add_enabled(
                finished,
                egui::Button::new(tr!(ui, "asteria-photos-uploads-clear-finished")),
            )
            .clicked()
        {
            uploads.clear_finished();
        }
    });
    if snap.summary.awaiting > 0 {
        ui.horizontal(|ui| {
            ui.label(tr!(
                ui,
                "asteria-photos-uploads-in-library",
                count = snap.summary.awaiting
            ));
            if ui
                .button(tr!(ui, "asteria-photos-uploads-keep-all"))
                .clicked()
            {
                uploads.decide_all(true);
            }
            if ui
                .button(tr!(ui, "asteria-photos-uploads-discard-all"))
                .clicked()
            {
                uploads.decide_all(false);
            }
        });
    }
    ui.separator();
}

/// What the engine says about itself, and what the latest drop left out.
fn notes(ui: &mut egui::Ui, snap: &UploadSnapshot) {
    let error_color = ui.visuals().error_fg_color;
    match &snap.state {
        UploadsState::Starting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr!(ui, "asteria-photos-uploads-starting"));
            });
        }
        UploadsState::Running => {}
        UploadsState::Waiting { error } => {
            let reason = error_text(ui.ctx(), error);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                tr!(ui, "asteria-photos-uploads-waiting", reason = reason),
            );
        }
        UploadsState::Stopped { error } => {
            let reason = error_text(ui.ctx(), error);
            ui.colored_label(
                error_color,
                tr!(ui, "asteria-photos-uploads-stopped", reason = reason),
            );
        }
        UploadsState::Unavailable { reason } => {
            ui.colored_label(
                error_color,
                tr!(
                    ui,
                    "asteria-photos-uploads-queue-unreadable",
                    reason = reason.as_str()
                ),
            );
        }
    }
    if snap.excluded > 0 {
        ui.weak(tr!(
            ui,
            "asteria-photos-uploads-excluded",
            count = snap.excluded
        ));
    }
    for w in &snap.warnings {
        let text = match w {
            PlanWarning::AmbiguousSidecar { path } => tr!(
                ui,
                "asteria-photos-uploads-warn-sidecar",
                path = path.display().to_string()
            ),
            PlanWarning::Unreadable { path, reason } => tr!(
                ui,
                "asteria-photos-uploads-warn-unreadable",
                path = path.display().to_string(),
                reason = reason.as_str()
            ),
        };
        ui.colored_label(ui.visuals().warn_fg_color, text);
    }
}

/// The files the library has that a waiting group's files duplicate.
fn existing_ui(ui: &mut egui::Ui, existing: &[String]) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.strong(tr!(ui, "asteria-photos-uploads-existing"));
        egui::ScrollArea::vertical()
            .id_salt("uploads-existing")
            .max_height(100.0)
            .show(ui, |ui| {
                for path in existing {
                    ui.label(path);
                }
            });
    });
}

fn list(ui: &mut egui::Ui, uploads: &Uploads, snap: &UploadSnapshot, view: &mut UploadsView) {
    let row_height = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
    egui::ScrollArea::vertical()
        .id_salt("uploads-list")
        .auto_shrink([false, false])
        .show_rows(ui, row_height, snap.groups.len(), |ui, range| {
            for group in &snap.groups[range] {
                row(ui, uploads, snap, group, view);
            }
        });
}

fn row(
    ui: &mut egui::Ui,
    uploads: &Uploads,
    snap: &UploadSnapshot,
    group: &QueuedGroup,
    view: &mut UploadsView,
) {
    let offered = actions(&group.state);
    let name = match group.files.as_slice() {
        [] => String::new(),
        [only] => only.inbox_path.clone(),
        [first, rest @ ..] => format!("{} (+{})", first.inbox_path, rest.len()),
    };
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if offered.cancel
                && ui
                    .button(tr!(ui, "asteria-photos-uploads-cancel"))
                    .clicked()
            {
                uploads.cancel(group.id);
            }
            if offered.retry && ui.button(tr!(ui, "asteria-photos-uploads-retry")).clicked() {
                uploads.retry(group.id);
            }
            if offered.discard
                && ui
                    .button(tr!(ui, "asteria-photos-uploads-discard"))
                    .clicked()
            {
                uploads.decide(group.id, false);
            }
            if offered.keep && ui.button(tr!(ui, "asteria-photos-uploads-keep")).clicked() {
                uploads.decide(group.id, true);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let image = icons::image(ui, icons::upload(), 14.0);
                ui.add(image);
                if offered.keep {
                    // A waiting row opens to show the files the library already has.
                    let open = view.opened == Some(group.id);
                    if ui
                        .add(egui::Button::selectable(open, name.as_str()).truncate())
                        .clicked()
                    {
                        view.opened = (!open).then_some(group.id);
                    }
                } else {
                    ui.add(egui::Label::new(name.as_str()).truncate());
                }
                ui.weak(state_text(ui, &group.state));
                if let Some((id, sent, total)) = snap.summary.active
                    && id == group.id
                    && total > 0
                {
                    let fraction = (sent as f32 / total as f32).clamp(0.0, 1.0);
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_width(120.0)
                            .show_percentage(),
                    );
                }
            });
        });
    });
}

fn state_text(ui: &egui::Ui, state: &GroupState) -> String {
    match state {
        GroupState::Hashing => tr!(ui, "asteria-photos-uploads-state-hashing"),
        GroupState::Checking => tr!(ui, "asteria-photos-uploads-state-checking"),
        GroupState::AwaitingDecision { .. } => tr!(ui, "asteria-photos-uploads-state-awaiting"),
        GroupState::Uploading { .. } => tr!(ui, "asteria-photos-uploads-state-uploading"),
        GroupState::Committing { .. } => tr!(ui, "asteria-photos-uploads-state-committing"),
        GroupState::Done { .. } => tr!(ui, "asteria-photos-uploads-state-done"),
        GroupState::Discarded => tr!(ui, "asteria-photos-uploads-state-discarded"),
        GroupState::Failed { reason } => tr!(
            ui,
            "asteria-photos-uploads-state-failed",
            reason = reason.as_str()
        ),
    }
}
