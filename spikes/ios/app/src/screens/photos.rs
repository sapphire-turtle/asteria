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
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    use super::{Answer, CHECKS, RECENT, Report, checklist, egui, reset_checklist};
    use crate::ios::{memory, photos};
    use crate::photos::{
        AssetInfo, Auth, FetchEvent, FetchResult, ResourceInfo, UploadPlan, mb_per_s, plan_upload,
        sample_line,
    };

    /// A read that has seen no data or download progress for this long is given up.
    const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    /// One listed asset and what reading its resources gave.
    struct Row {
        asset: AssetInfo,
        plan: Option<UploadPlan>,
        /// Where the planned primary resource sits among the asset's resources.
        primary_index: Option<usize>,
        /// Where the planned paired video sits, for a Live Photo.
        paired_index: Option<usize>,
        /// The report lines of the last reads of the primary and the paired video.
        primary_result: Option<String>,
        pair_result: Option<String>,
    }

    impl Row {
        /// The resource a job reads: its index and what `recent` listed there.
        fn resource(&self, pair: bool) -> Option<(usize, &ResourceInfo)> {
            let index = if pair {
                self.paired_index
            } else {
                self.primary_index
            }?;
            Some((index, self.asset.resources.get(index)?))
        }
    }

    /// One resource to read: the primary or the paired video of a row.
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Job {
        row: usize,
        pair: bool,
    }

    impl Job {
        fn report_key(self) -> String {
            let suffix = if self.pair { ".pair" } else { "" };
            format!("photos.fetch.{:02}{suffix}", self.row)
        }
    }

    /// The read in flight. Each read has its own channel, so events from a read that was
    /// given up go nowhere.
    struct InFlight {
        job: Job,
        rx: Receiver<FetchEvent>,
        request: Option<photos::RequestId>,
        bytes: u64,
        /// The iCloud download's progress, once PhotoKit reports one.
        download: Option<f64>,
        last_activity: Instant,
        started: Instant,
        footprint_before: Option<u64>,
        footprint_peak: Option<u64>,
    }

    /// Totals of "30 件を読む" so far.
    struct Batch {
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
        auth_tx: std::sync::mpsc::Sender<Auth>,
        auth_rx: Receiver<Auth>,
        /// Whether the counts and the rows reflect the current authorization.
        loaded: bool,
        counts: Option<(usize, usize, usize)>,
        rows: Vec<Row>,
        /// At most one read is in flight; the rest wait here.
        queue: VecDeque<Job>,
        in_flight: Option<InFlight>,
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
            Self {
                ctx: ctx.clone(),
                answers: vec![Answer::Unchecked; CHECKS.len()],
                auth,
                auth_tx,
                auth_rx,
                loaded: false,
                counts: None,
                rows: Vec::new(),
                queue: VecDeque::new(),
                in_flight: None,
                batch: None,
            }
        }

        fn idle(&self) -> bool {
            self.in_flight.is_none() && self.queue.is_empty()
        }

        /// Draw the screen, handling what PhotoKit sent since the last frame.
        pub fn ui(&mut self, ui: &mut egui::Ui, report: &mut Report) {
            self.receive(report);
            if !self.loaded && self.idle() && can_read(self.auth) {
                self.load(report);
            }
            if self.in_flight.is_some() {
                // Keeps the timeout check running while nothing arrives.
                self.ctx.request_repaint_after(Duration::from_secs(1));
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
                        let idle = self.idle();
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
                            self.batch = Some(Batch {
                                started: Instant::now(),
                                reads: 0,
                                bytes: 0,
                                errors: 0,
                            });
                            self.enqueue(0..self.rows.len());
                            self.next(report);
                        }
                    });
                    if let Some((images, videos, live)) = self.counts {
                        ui.monospace(format!("画像 {images} / 動画 {videos} / Live {live}"));
                    }
                    if let Some(batch) = &self.batch {
                        ui.monospace(format!(
                            "まとめて読んでいます: {} 件読み終え、{} bytes、残り {} 件",
                            batch.reads,
                            batch.bytes,
                            self.queue.len()
                        ));
                    }
                    ui.separator();
                    checklist(ui, "photos", CHECKS, &mut self.answers, report);
                    ui.separator();
                    let idle = self.idle();
                    let mut start = None;
                    for (n, row) in self.rows.iter().enumerate() {
                        let in_flight = self.in_flight.as_ref().filter(|f| f.job.row == n);
                        if row_ui(ui, n, row, in_flight, idle) {
                            start = Some(n);
                        }
                        ui.separator();
                    }
                    if let Some(n) = start {
                        self.enqueue(n..n + 1);
                        self.next(report);
                    }
                });
        }

        /// Apply the authorization answers and read events that arrived, and give up a
        /// read that has gone quiet for too long.
        fn receive(&mut self, report: &mut Report) {
            while let Ok(auth) = self.auth_rx.try_recv() {
                self.auth = auth;
                self.loaded = false;
                report.set("photos.auth", auth.name());
            }
            let mut done = None;
            if let Some(flight) = &mut self.in_flight {
                while let Ok(event) = flight.rx.try_recv() {
                    flight.last_activity = Instant::now();
                    flight.footprint_peak = flight.footprint_peak.max(memory::footprint());
                    match event {
                        FetchEvent::Progress(bytes) => flight.bytes = bytes,
                        FetchEvent::Downloading(fraction) => flight.download = Some(fraction),
                        FetchEvent::Done(result) => {
                            done = Some(result);
                            break;
                        }
                    }
                }
                if done.is_none() && flight.last_activity.elapsed() >= READ_TIMEOUT {
                    if let Some(request) = flight.request {
                        photos::cancel(request);
                    }
                    done = Some(FetchResult {
                        bytes: flight.bytes,
                        hash: String::new(),
                        secs: flight.started.elapsed().as_secs_f64(),
                        error: Some("timeout".to_owned()),
                        from_icloud: flight.download.is_some(),
                    });
                }
            }
            if let Some(result) = done
                && let Some(flight) = self.in_flight.take()
            {
                self.finish(flight, result, report);
                self.next(report);
            }
        }

        /// Record the end of `flight` in its row, the batch totals and `report`.
        fn finish(&mut self, flight: InFlight, result: FetchResult, report: &mut Report) {
            let after = memory::footprint();
            let peak = flight.footprint_peak.max(after);
            let line = format!(
                "{}, footprint before={} peak={} after={}",
                result.summary(),
                mb(flight.footprint_before),
                mb(peak),
                mb(after),
            );
            report.set(&flight.job.report_key(), &line);
            if let Some(batch) = &mut self.batch {
                batch.reads += 1;
                batch.bytes += result.bytes;
                batch.errors += usize::from(result.error.is_some());
            }
            if let Some(row) = self.rows.get_mut(flight.job.row) {
                if flight.job.pair {
                    row.pair_result = Some(line);
                } else {
                    row.primary_result = Some(line);
                }
            }
        }

        /// Queue the primary and, for a Live Photo, the paired video of each row in
        /// `rows`.
        fn enqueue(&mut self, rows: std::ops::Range<usize>) {
            for n in rows {
                let Some(row) = self.rows.get(n) else {
                    continue;
                };
                if row.primary_index.is_some() {
                    self.queue.push_back(Job {
                        row: n,
                        pair: false,
                    });
                }
                if row.paired_index.is_some() {
                    self.queue.push_back(Job { row: n, pair: true });
                }
            }
        }

        /// Start the next queued read, or record the batch in `report` once the queue is
        /// empty.
        fn next(&mut self, report: &mut Report) {
            if self.in_flight.is_some() {
                return;
            }
            while let Some(job) = self.queue.pop_front() {
                let Some(row) = self.rows.get(job.row) else {
                    continue;
                };
                let Some((index, expected)) = row.resource(job.pair) else {
                    continue;
                };
                let (tx, rx) = channel();
                let footprint_before = memory::footprint();
                let request = photos::fetch_original(
                    &row.asset.local_id,
                    index,
                    expected,
                    tx,
                    self.ctx.clone(),
                );
                let now = Instant::now();
                self.in_flight = Some(InFlight {
                    job,
                    rx,
                    request,
                    bytes: 0,
                    download: None,
                    last_activity: now,
                    started: now,
                    footprint_before,
                    footprint_peak: footprint_before,
                });
                return;
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

        /// Count the library and list the newest assets, into the screen and `report`,
        /// with how long that took on the main thread.
        fn load(&mut self, report: &mut Report) {
            let started = Instant::now();
            self.loaded = true;
            let (images, videos, live) = photos::counts();
            self.counts = Some((images, videos, live));
            self.rows = photos::recent(RECENT)
                .into_iter()
                .map(|asset| {
                    let plan = plan_upload(&asset);
                    let index_of = |resource: &ResourceInfo| {
                        asset.resources.iter().position(|r| r == resource)
                    };
                    let primary_index = plan.as_ref().and_then(|p| index_of(&p.primary.0));
                    let paired_index = plan
                        .as_ref()
                        .and_then(|p| p.paired_video.as_ref())
                        .and_then(|(video, _)| index_of(video));
                    Row {
                        asset,
                        plan,
                        primary_index,
                        paired_index,
                        primary_result: None,
                        pair_result: None,
                    }
                })
                .collect();
            let load_ms = started.elapsed().as_millis();
            report.set(
                "photos.counts",
                format!("images={images} videos={videos} live={live} load_ms={load_ms}"),
            );
            for (n, row) in self.rows.iter().enumerate() {
                report.set(&format!("photos.sample.{n:02}"), sample_line(&row.asset));
            }
        }
    }

    /// Whether the authorization lets the app see any photos.
    fn can_read(auth: Auth) -> bool {
        matches!(auth, Auth::Authorized | Auth::Limited)
    }

    /// A footprint in megabytes for the report, or `?` if it could not be read.
    fn mb(bytes: Option<u64>) -> String {
        bytes.map_or("?".to_owned(), |b| {
            format!("{:.1}MB", b as f64 / 1_000_000.0)
        })
    }

    /// One asset: its date and Live mark, its resources, the planned names, and a button
    /// to read its originals (enabled when `idle`). Returns whether the button was pressed.
    fn row_ui(
        ui: &mut egui::Ui,
        n: usize,
        row: &Row,
        in_flight: Option<&InFlight>,
        idle: bool,
    ) -> bool {
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
        for (pair, result) in [(false, &row.primary_result), (true, &row.pair_result)] {
            let label = if pair { "動画" } else { "原本" };
            match in_flight.filter(|f| f.job.pair == pair) {
                Some(flight) => {
                    let download = flight.download.map_or(String::new(), |f| {
                        format!("、iCloud から {:.0}%", f * 100.0)
                    });
                    ui.monospace(format!(
                        "{label}: 読んでいます {} bytes{download}",
                        flight.bytes
                    ));
                }
                None => {
                    if let Some(line) = result {
                        ui.monospace(format!("{label}: {line}"));
                    }
                }
            }
        }
        pressed
    }
}
