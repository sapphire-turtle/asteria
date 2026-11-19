//! The status bar's notice: a line that tells something for a while ([`NOTICE_FOR`]) and
//! then goes, such as a refused drop or a rating the server would not apply.
//!
//! Wide, the app draws it on its status bar ([`StatusNotice::ui`]). Narrow, there is no status
//! bar, so it sits at the foot of the screen ([`StatusNotice::foot_ui`]).

use std::time::{Duration, Instant};

use asteria_common_api::RejectReason;
use corundum_framework::egui;
use corundum_framework::gui::tr;

use crate::queue::Notice;
use crate::session::Session;

/// How long the status bar keeps a notice.
pub const NOTICE_FOR: Duration = Duration::from_secs(8);

/// The notice slot: the words, and since when they are shown.
#[derive(Clone, Debug, Default)]
pub struct StatusNotice(Option<(String, Instant)>);

impl StatusNotice {
    /// Tell `text` from now on (anew, even if it is what is shown).
    pub fn set(&mut self, text: impl Into<String>) {
        self.set_at(text, Instant::now());
    }

    fn set_at(&mut self, text: impl Into<String>, since: Instant) {
        self.0 = Some((text.into(), since));
    }

    /// Tell what arrived at once as one notice, joined with spaces; with nothing, keep what
    /// is shown.
    pub fn set_all(&mut self, texts: Vec<String>) {
        if !texts.is_empty() {
            self.set(texts.join(" "));
        }
    }

    /// Takes what `session`'s rating queue has to tell, and tells it.
    pub fn take_ratings(&mut self, ctx: &egui::Context, session: &mut Session) {
        let texts = session
            .take_notices()
            .iter()
            .map(|n| rating_notice(ctx, n))
            .collect();
        self.set_all(texts);
    }

    /// Stop telling it.
    pub fn clear(&mut self) {
        self.0 = None;
    }

    /// The words shown, until they go.
    pub fn text(&self) -> Option<&str> {
        let (text, since) = self.0.as_ref()?;
        (since.elapsed() < NOTICE_FOR).then_some(text.as_str())
    }

    /// When the notice was set (a new one, even with the same words, sets it again), gone or
    /// not. For tests.
    pub fn since(&self) -> Option<Instant> {
        self.0.as_ref().map(|(_, since)| *since)
    }

    /// Draws it in `ui` (the status bar), until it goes; then forgets it.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let Some((text, since)) = &self.0 else {
            return;
        };
        match NOTICE_FOR.checked_sub(since.elapsed()) {
            Some(left) => {
                ui.colored_label(ui.visuals().warn_fg_color, text.as_str());
                ui.ctx().request_repaint_after(left);
            }
            None => self.0 = None,
        }
    }

    /// Narrow, where there is no status bar: draws it in a panel at the foot of `ui`, named by
    /// `id`. Call it before the screen's other panels at the foot that it is to sit above.
    pub fn foot_ui(&mut self, ui: &mut egui::Ui, id: impl egui::AsIdSalt) {
        if self.0.is_some() {
            egui::Panel::bottom(id).show(ui, |ui| self.ui(ui));
        }
    }
}

/// The words for what the rating queue has to tell.
pub fn rating_notice(ctx: &egui::Context, notice: &Notice) -> String {
    match notice {
        Notice::RatingRejected { reason, .. } => {
            let key = match reason {
                RejectReason::NotFound => "asteria-common-client-rating-reason-not-found",
                RejectReason::ReadOnly => "asteria-common-client-rating-reason-read-only",
                RejectReason::Invalid => "asteria-common-client-rating-reason-invalid",
            };
            let reason = tr!(ctx, key);
            tr!(
                ctx,
                "asteria-common-client-rating-rejected",
                reason = reason
            )
        }
        Notice::RatingNotSaved { .. } => tr!(ctx, "asteria-common-client-rating-not-saved"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn several_notices_at_once_are_one_and_none_keeps_the_last() {
        let mut n = StatusNotice::default();
        assert_eq!(n.text(), None);
        n.set_all(vec!["a".into(), "b".into()]);
        assert_eq!(n.text(), Some("a b"));
        let since = n.since();
        assert!(since.is_some());
        n.set_all(vec![]);
        assert_eq!((n.text(), n.since()), (Some("a b"), since));
        n.set("c");
        assert_eq!(n.text(), Some("c"));
        assert!(n.since() >= since);
        n.clear();
        assert_eq!((n.text(), n.since()), (None, None));
    }

    #[test]
    fn a_notice_goes_after_its_time() {
        let mut n = StatusNotice::default();
        n.set_at("old", Instant::now() - NOTICE_FOR);
        assert_eq!(n.text(), None);
        assert!(n.since().is_some());
    }
}
