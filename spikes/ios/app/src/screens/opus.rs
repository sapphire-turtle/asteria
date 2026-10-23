//! The Opus screen (spec section 8, item 3): time each pure-Rust decoder on the device
//! and report x_realtime, the PCM hash and any error. SNR is judged on Linux against
//! libopus (see the opus-probe table); this screen only needs the speed.

use std::sync::mpsc::{Receiver, TryRecvError, channel};

use corundum_gui::egui;
use opus_probe::{Candidate, Measurement, SAMPLE, measure, read_ogg_opus};

use crate::report::Report;

/// How many times each candidate is decoded; the median x_realtime is reported.
const RUNS: usize = 3;

/// Whether this is an optimized build; debug numbers say nothing about the 20x bar.
const RELEASE: bool = !cfg!(debug_assertions);

/// One candidate's outcome over [`RUNS`] decodes.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    name: &'static str,
    /// Median x_realtime over the successful runs; `None` when every run failed.
    x_realtime: Option<f64>,
    pcm_hash: Option<String>,
    error: Option<String>,
}

/// State of the Opus screen.
pub struct OpusScreen {
    rows: Vec<Row>,
    /// Results of the running measurement; `None` when idle.
    pending: Option<Receiver<Vec<Row>>>,
}

impl OpusScreen {
    /// A fresh screen; the build kind is recorded in `report` right away.
    pub fn new(report: &mut Report) -> Self {
        if !RELEASE {
            report.set("opus.build", "debug");
        }
        Self {
            rows: Vec::new(),
            pending: None,
        }
    }

    /// Draw the screen; a finished measurement is written to `report`.
    pub fn ui(&mut self, ui: &mut egui::Ui, report: &mut Report) {
        self.poll(report);
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.heading("Opus");
                if !RELEASE {
                    ui.colored_label(ui.visuals().warn_fg_color, "Debug ビルドの値です");
                }
                let running = self.pending.is_some();
                if ui
                    .add_enabled(!running, egui::Button::new("計測する"))
                    .clicked()
                {
                    self.start(ui.ctx().clone());
                }
                if running {
                    ui.label("計測中…");
                }
                for row in &self.rows {
                    ui.separator();
                    ui.strong(row.name);
                    match (&row.error, row.x_realtime) {
                        (Some(e), _) => ui.label(format!("エラー: {e}")),
                        (None, Some(x)) => ui.monospace(format!("{x:.1}x リアルタイム")),
                        (None, None) => ui.label("結果なし"),
                    };
                    if let Some(hash) = &row.pcm_hash {
                        ui.monospace(format!("pcm_hash: {hash}"));
                    }
                }
            });
    }

    /// Start measuring every candidate on a separate thread.
    fn start(&mut self, ctx: egui::Context) {
        let (tx, rx) = channel();
        self.pending = Some(rx);
        std::thread::spawn(move || {
            let rows = run_all();
            // The screen may be gone; nothing to do then.
            let _ = tx.send(rows);
            ctx.request_repaint();
        });
    }

    /// Take the finished measurement, if any, and record it.
    fn poll(&mut self, report: &mut Report) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(rows) => {
                for row in &rows {
                    record(row, report);
                }
                self.rows = rows;
                self.pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // The thread died without a result (a panic outside `measure`).
                report.set("opus.error", "measurement thread ended without a result");
                self.pending = None;
            }
        }
    }
}

/// Write `row` under `opus.<name>.*`.
fn record(row: &Row, report: &mut Report) {
    if let Some(x) = row.x_realtime {
        report.set(&format!("opus.{}.x_realtime", row.name), format!("{x:.1}"));
    }
    if let Some(hash) = &row.pcm_hash {
        report.set(&format!("opus.{}.pcm_hash", row.name), hash);
    }
    if let Some(error) = &row.error {
        report.set(&format!("opus.{}.error", row.name), error);
    }
}

/// Decode the sample [`RUNS`] times with every candidate.
fn run_all() -> Vec<Row> {
    let ogg = match read_ogg_opus(SAMPLE) {
        Ok(ogg) => ogg,
        Err(e) => {
            return Candidate::ALL
                .iter()
                .map(|c| Row {
                    name: c.name(),
                    x_realtime: None,
                    pcm_hash: None,
                    error: Some(e.to_string()),
                })
                .collect();
        }
    };
    Candidate::ALL
        .iter()
        .map(|&c| {
            summarize(
                c.name(),
                (0..RUNS).map(|_| measure(c, &ogg, None)).collect(),
            )
        })
        .collect()
}

/// Fold one candidate's runs into a row: the median x_realtime of the successful runs,
/// the first run's hash, and the first error.
fn summarize(name: &'static str, runs: Vec<Measurement>) -> Row {
    let speeds: Vec<f64> = runs
        .iter()
        .filter(|m| m.error.is_none())
        .map(|m| m.x_realtime)
        .collect();
    Row {
        name,
        x_realtime: median(speeds),
        pcm_hash: runs.iter().find_map(|m| m.pcm_hash.clone()),
        error: runs.iter().find_map(|m| m.error.clone()),
    }
}

/// The median of `values` (the mean of the middle two for an even count).
fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(x: f64, hash: Option<&str>, error: Option<&str>) -> Measurement {
        Measurement {
            candidate: Candidate::OpusPure,
            audio_secs: 1.0,
            decode_secs: 1.0 / x.max(1.0),
            x_realtime: x,
            snr_db: None,
            pcm_hash: hash.map(str::to_owned),
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn median_of_odd_even_and_empty() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(vec![]), None);
    }

    #[test]
    fn summarize_takes_median_and_first_hash() {
        let row = summarize(
            "x",
            vec![
                run(10.0, Some("a"), None),
                run(30.0, Some("a"), None),
                run(20.0, Some("a"), None),
            ],
        );
        assert_eq!(row.x_realtime, Some(20.0));
        assert_eq!(row.pcm_hash.as_deref(), Some("a"));
        assert_eq!(row.error, None);
    }

    #[test]
    fn summarize_reports_failures() {
        let row = summarize(
            "x",
            vec![run(0.0, None, Some("boom")), run(0.0, None, None)],
        );
        assert_eq!(row.error.as_deref(), Some("boom"));
        let row = summarize("x", vec![run(0.0, None, Some("boom"))]);
        assert_eq!(row.x_realtime, None);
    }

    #[test]
    fn the_sample_decodes_with_every_candidate() {
        let rows = run_all();
        assert_eq!(rows.len(), Candidate::ALL.len());
        for row in rows {
            assert_eq!(row.error, None, "{}", row.name);
            assert!(row.pcm_hash.is_some(), "{}", row.name);
        }
    }

    #[test]
    fn record_writes_only_what_exists() {
        let mut report = Report::default();
        record(
            &Row {
                name: "n",
                x_realtime: Some(12.34),
                pcm_hash: Some("h".into()),
                error: None,
            },
            &mut report,
        );
        let text = report.to_text();
        assert!(text.contains("opus.n.x_realtime: 12.3"));
        assert!(text.contains("opus.n.pcm_hash: h"));
        assert!(!text.contains("opus.n.error"));
    }
}
