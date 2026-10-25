//! The report screen: the report as text, and a button to copy it off the device.

use corundum_framework::egui;

use crate::report::Report;

/// Show the report in a monospace, scrollable block with a copy button.
pub fn ui(ui: &mut egui::Ui, report: &Report) {
    let text = report.to_text();
    ui.heading("Report");
    if ui.button("コピー").clicked() {
        crate::copy_report(ui.ctx(), &text);
    }
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.add(egui::Label::new(egui::RichText::new(&text).monospace()).selectable(true));
    });
}
