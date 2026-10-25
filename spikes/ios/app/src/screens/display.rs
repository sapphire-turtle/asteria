//! The display and touch screen (spec section 8, item 1): safe area, screen size, scale,
//! Japanese text, taps, multi-touch, scrolling and the soft keyboard, each with an
//! OK/NG answer that goes into the report.

use std::collections::BTreeSet;

use corundum_framework::egui::{self, Event, TouchId, TouchPhase};

use super::{Answer, checklist};
use crate::report::Report;

/// The things the person holding the device confirms, as `(report key, label)`.
const CHECKS: &[(&str, &str)] = &[
    ("safe_area", "セーフエリア（ノッチの下に潜らない）"),
    ("tap", "タップ"),
    ("scroll", "スクロール"),
    ("keyboard", "キーボード（入力欄が隠れない）"),
    ("cjk", "日本語の表示"),
    ("rotate", "画面の回転"),
];

/// State of the display screen.
pub struct DisplayScreen {
    taps: u32,
    text: String,
    answers: Vec<Answer>,
    /// Fingers currently on the screen.
    touches: BTreeSet<u64>,
}

impl DisplayScreen {
    /// A fresh screen; every check starts as `unchecked` in `report`.
    pub fn new(report: &mut Report) -> Self {
        super::reset_checklist("display", CHECKS, report);
        Self {
            taps: 0,
            text: String::new(),
            answers: vec![Answer::Unchecked; CHECKS.len()],
            touches: BTreeSet::new(),
        }
    }

    /// Per frame, whichever screen is open: track the fingers down and record the display
    /// measurements in `report`.
    pub fn update(&mut self, ctx: &egui::Context, report: &mut Report) {
        ctx.input(|input| {
            for event in &input.events {
                if let Event::Touch {
                    id: TouchId(id),
                    phase,
                    ..
                } = event
                {
                    match phase {
                        TouchPhase::Start | TouchPhase::Move => self.touches.insert(*id),
                        TouchPhase::End | TouchPhase::Cancel => self.touches.remove(id),
                    };
                }
            }
        });
        report.set("display.safe_area", safe_area_text(ctx));
        report.set("display.screen_rect", rect_text(ctx.viewport_rect()));
        report.set("display.ppp", ctx.pixels_per_point());
    }

    /// Draw the screen, writing check answers to `report`.
    pub fn ui(&mut self, ui: &mut egui::Ui, report: &mut Report) {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.heading("Display");
                let ctx = ui.ctx().clone();
                ui.monospace(format!("safe area: {}", safe_area_text(&ctx)));
                ui.monospace(format!("screen:    {}", rect_text(ctx.viewport_rect())));
                ui.monospace(format!("content:   {}", rect_text(ctx.content_rect())));
                ui.monospace(format!("ppp:       {}", ctx.pixels_per_point()));
                ui.separator();
                ui.label("日本語の表示：漢字・ひらがな・カタカナ");
                ui.separator();

                if ui.button(format!("タップ: {}", self.taps)).clicked() {
                    self.taps += 1;
                }
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 120.0),
                    egui::Sense::click_and_drag(),
                );
                ui.painter().rect_stroke(
                    rect,
                    4.0,
                    ui.visuals().widgets.noninteractive.fg_stroke,
                    egui::StrokeKind::Inside,
                );
                let pos = ctx.input(|i| i.pointer.latest_pos());
                let label = match pos {
                    Some(p) => {
                        format!("指: ({:.0}, {:.0})\n同時: {}", p.x, p.y, self.touches.len())
                    }
                    None => format!("指: なし\n同時: {}", self.touches.len()),
                };
                ui.painter().text(
                    rect.left_top() + egui::vec2(8.0, 8.0),
                    egui::Align2::LEFT_TOP,
                    label,
                    egui::FontId::monospace(14.0),
                    ui.visuals().text_color(),
                );
                ui.separator();

                egui::ScrollArea::vertical()
                    .id_salt("list")
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for n in 1..=200 {
                            ui.label(format!("行 {n}"));
                        }
                    });
                ui.separator();
                ui.add(egui::TextEdit::singleline(&mut self.text).hint_text("ここに入力"));
                ui.separator();

                checklist(ui, "display", CHECKS, &mut self.answers, report);
            });
    }
}

/// The safe-area insets as `top/right/bottom/left` in points.
fn safe_area_text(ctx: &egui::Context) -> String {
    let m = ctx.input(|i| i.safe_area_insets()).0;
    format!(
        "t{:.0} r{:.0} b{:.0} l{:.0}",
        m.top, m.right, m.bottom, m.left
    )
}

fn rect_text(rect: egui::Rect) -> String {
    format!(
        "({:.0}, {:.0}) - ({:.0}, {:.0})",
        rect.min.x, rect.min.y, rect.max.x, rect.max.y
    )
}
