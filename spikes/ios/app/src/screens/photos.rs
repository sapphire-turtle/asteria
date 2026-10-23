//! The photos screen (spec section 8, item 3): the library authorization and a button to
//! ask for it, how many images, videos and Live Photos the app sees, the 30 newest assets
//! with the names [`plan_upload`](crate::photos::plan_upload) would send them under, and
//! streamed reads of their originals, one by one or all 30 in a row. PhotoKit exists only
//! on iOS; elsewhere the screen says so.

use corundum_gui::egui;

use super::{Answer, checklist, reset_checklist};
use crate::report::Report;

/// The things the person holding the device confirms, as `(report key, label)`.
const CHECKS: &[(&str, &str)] = &[
    ("denied_ok", "「許可しない」でも落ちず、状態が出る"),
    ("limited_ok", "「選択した写真のみ」でも落ちず、状態が出る"),
    ("heic_original", "HEIC の原本が読める"),
    ("live_pair", "Live Photos が写真と動画の組になる"),
    ("video_4k", "4K 動画をメモリを増やさずに読める"),
    ("icloud_only", "iCloud にしかない原本を読める"),
];

/// How many of the newest assets the screen lists.
#[cfg(target_os = "ios")]
const RECENT: usize = 30;

/// State of the photos screen on platforms without PhotoKit.
#[cfg(not(target_os = "ios"))]
pub struct PhotosScreen {
    answers: Vec<Answer>,
}

#[cfg(not(target_os = "ios"))]
impl PhotosScreen {
    /// A fresh screen; every check starts as `unchecked` in `report`.
    pub fn new(_ctx: &egui::Context, report: &mut Report) -> Self {
        reset_checklist("photos", CHECKS, report);
        report.set("photos.auth", "unavailable");
        Self {
            answers: vec![Answer::Unchecked; CHECKS.len()],
        }
    }

    /// Draw the screen: PhotoKit is iOS only, so there is only the checklist.
    pub fn ui(&mut self, ui: &mut egui::Ui, report: &mut Report) {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.heading("Photos");
                ui.label("iOS のみ");
                ui.separator();
                checklist(ui, "photos", CHECKS, &mut self.answers, report);
            });
    }
}

#[cfg(target_os = "ios")]
pub use ios_screen::PhotosScreen;

#[cfg(target_os = "ios")]
mod ios_screen {
    use std::collections::VecDeque;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::Instant;

    use super::{Answer, CHECKS, RECENT, Report, checklist, egui, reset_checklist};
    use crate::ios::photos;
    use crate::photos::{
        AssetInfo, Auth, FetchEvent, FetchResult, UploadPlan, mb_per_s, plan_upload, sample_line,
    };

    /// One listed asset and what reading its original gave.
    struct Row {
        asset: AssetInfo,
        plan: Option<UploadPlan>,
        /// Where the planned primary resource sits among the asset's resources.
        primary_index: Option<usize>,
        result: Option<FetchResult>,
    }

    /// "30 件を読む" in progress: the rows still to read and the totals so far.
    struct Batch {
        queue: VecDeque<usize>,
        started: Instant,
        reads: usize,
        bytes: u64,
        errors: usize,
    }

    /// State of the photos screen.
    pub struct PhotosScreen {
        ctx: egui::Context,
        answers: Vec<Answer>,
        auth: Auth,
        auth_tx: Sender<Auth>,
        auth_rx: Receiver<Auth>,
        fetch_tx: Sender<FetchEvent>,
        fetch_rx: Receiver<FetchEvent>,
        /// Whether the counts and the rows reflect the current authorization.
        loaded: bool,
        counts: Option<(usize, usize, usize)>,
        rows: Vec<Row>,
        /// The row whose original is being read, and the bytes read so far. At most one
        /// read is in flight, so PhotoKit's events need no tag.
        reading: Option<(usize, u64)>,
        batch: Option<Batch>,
    }

    impl PhotosScreen {
        /// A fresh screen showing the current authorization (without prompting); every
        /// check starts as `unchecked` in `report`.
        pub fn new(ctx: &egui::Context, report: &mut Report) -> Self {
            reset_checklist("photos", CHECKS, report);
            let auth = photos::current_auth();
            report.set("photos.auth", auth.name());
            let (auth_tx, auth_rx) = channel();
            let (fetch_tx, fetch_rx) = channel();
            Self {
                ctx: ctx.clone(),
                answers: vec![Answer::Unchecked; CHECKS.len()],
                auth,
                auth_tx,
                auth_rx,
                fetch_tx,
                fetch_rx,
                loaded: false,
                counts: None,
                rows: Vec::new(),
                reading: None,
                batch: None,
            }
        }

        /// Draw the screen, handling what PhotoKit sent since the last frame.
        pub fn ui(&mut self, ui: &mut egui::Ui, report: &mut Report) {
            self.receive(report);
            if !self.loaded && self.reading.is_none() && can_read(self.auth) {
                self.load(report);
            }
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.heading("Photos");
                    ui.label(format!("許可: {}", self.auth.describe()));
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("許可を求める").clicked() {
                            photos::request_auth(self.auth_tx.clone(), self.ctx.clone());
                        }
                        let idle = self.reading.is_none();
                        if ui
                            .add_enabled(
                                idle && can_read(self.auth),
                                egui::Button::new("読み込み直す"),
                            )
                            .clicked()
                        {
                            self.load(report);
                        }
                        if ui
                            .add_enabled(
                                idle && !self.rows.is_empty(),
                                egui::Button::new(format!("{RECENT} 件を読む")),
                            )
                            .clicked()
                        {
                            self.start_batch(report);
                        }
                    });
                    if let Some((images, videos, live)) = self.counts {
                        ui.monospace(format!("画像 {images} / 動画 {videos} / Live {live}"));
                    }
                    if let Some(batch) = &self.batch {
                        ui.monospace(format!(
                            "まとめて読んでいます: {} 件目、{} bytes",
                            batch.reads + 1,
                            batch.bytes
                        ));
                    }
                    ui.separator();
                    checklist(ui, "photos", CHECKS, &mut self.answers, report);
                    ui.separator();
                    let mut start = None;
                    for (n, row) in self.rows.iter().enumerate() {
                        let reading = self.reading.filter(|(r, _)| *r == n).map(|(_, b)| b);
                        if row_ui(ui, n, row, reading, self.reading.is_none()) {
                            start = Some(n);
                        }
                        ui.separator();
                    }
                    if let Some(n) = start {
                        self.start(n);
                    }
                });
        }

        /// Apply the authorization answers and read events that arrived.
        fn receive(&mut self, report: &mut Report) {
            while let Ok(auth) = self.auth_rx.try_recv() {
                self.auth = auth;
                self.loaded = false;
                report.set("photos.auth", auth.name());
            }
            while let Ok(event) = self.fetch_rx.try_recv() {
                let Some((n, _)) = self.reading else {
                    continue;
                };
                match event {
                    FetchEvent::Progress(bytes) => self.reading = Some((n, bytes)),
                    FetchEvent::Done(result) => {
                        self.reading = None;
                        report.set(&format!("photos.fetch.{n:02}"), result.summary());
                        if let Some(batch) = &mut self.batch {
                            batch.reads += 1;
                            batch.bytes += result.bytes;
                            batch.errors += usize::from(result.error.is_some());
                        }
                        if let Some(row) = self.rows.get_mut(n) {
                            row.result = Some(result);
                        }
                        self.next_in_batch(report);
                    }
                }
            }
        }

        /// Count the library and list the newest assets, into the screen and `report`.
        fn load(&mut self, report: &mut Report) {
            self.loaded = true;
            let (images, videos, live) = photos::counts();
            self.counts = Some((images, videos, live));
            report.set(
                "photos.counts",
                format!("images={images} videos={videos} live={live}"),
            );
            self.rows = photos::recent(RECENT)
                .into_iter()
                .map(|asset| {
                    let plan = plan_upload(&asset);
                    let primary_index = plan
                        .as_ref()
                        .and_then(|plan| asset.resources.iter().position(|r| *r == plan.primary.0));
                    Row {
                        asset,
                        plan,
                        primary_index,
                        result: None,
                    }
                })
                .collect();
            for (n, row) in self.rows.iter().enumerate() {
                report.set(&format!("photos.sample.{n:02}"), sample_line(&row.asset));
            }
        }

        /// Start reading the primary resource of row `n`. Returns whether it started.
        fn start(&mut self, n: usize) -> bool {
            let Some(row) = self.rows.get(n) else {
                return false;
            };
            let Some(index) = row.primary_index else {
                return false;
            };
            self.reading = Some((n, 0));
            photos::fetch_original(
                &row.asset.local_id,
                index,
                self.fetch_tx.clone(),
                self.ctx.clone(),
            );
            true
        }

        fn start_batch(&mut self, report: &mut Report) {
            self.batch = Some(Batch {
                queue: (0..self.rows.len()).collect(),
                started: Instant::now(),
                reads: 0,
                bytes: 0,
                errors: 0,
            });
            self.next_in_batch(report);
        }

        /// Start the batch's next readable row, or record the batch in `report` if none
        /// is left.
        fn next_in_batch(&mut self, report: &mut Report) {
            while let Some(n) = self.batch.as_mut().and_then(|b| b.queue.pop_front()) {
                if self.start(n) {
                    return;
                }
            }
            if let Some(batch) = self.batch.take() {
                let secs = batch.started.elapsed().as_secs_f64();
                report.set(
                    "photos.batch",
                    format!(
                        "{} reads, {} bytes, {secs:.2} s, {:.1} MB/s, {} errors",
                        batch.reads,
                        batch.bytes,
                        mb_per_s(batch.bytes, secs),
                        batch.errors,
                    ),
                );
            }
        }
    }

    /// Whether the authorization lets the app see any photos.
    fn can_read(auth: Auth) -> bool {
        matches!(auth, Auth::Authorized | Auth::Limited)
    }

    /// One asset: its date and Live mark, its resources, the planned names, and a button
    /// to read its original (enabled when `idle`). Returns whether the button was pressed.
    fn row_ui(ui: &mut egui::Ui, n: usize, row: &Row, reading: Option<u64>, idle: bool) -> bool {
        let asset = &row.asset;
        ui.label(format!(
            "{n:02} {}{}",
            if asset.is_live { "[LIVE] " } else { "" },
            asset.created.as_deref().unwrap_or("日付なし"),
        ));
        for resource in &asset.resources {
            ui.monospace(format!(
                "  {:?} {}",
                resource.kind, resource.original_filename
            ));
        }
        let plan = match &row.plan {
            Some(UploadPlan {
                primary,
                paired_video: Some(video),
            }) => format!("送る名前: {} + {}", primary.1, video.1),
            Some(UploadPlan { primary, .. }) => format!("送る名前: {}", primary.1),
            None => "送るものなし".to_owned(),
        };
        ui.label(plan);
        let pressed = ui
            .add_enabled(
                idle && row.primary_index.is_some(),
                egui::Button::new("原本を読む"),
            )
            .clicked();
        if let Some(bytes) = reading {
            ui.monospace(format!("読んでいます: {bytes} bytes"));
        } else if let Some(result) = &row.result {
            ui.monospace(result.summary());
        }
        pressed
    }
}
