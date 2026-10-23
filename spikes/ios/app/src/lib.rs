//! A spike app for iOS: what egui/eframe, corundum-gui and the platform do on a real
//! device (display, touch, audio, photos). The screens live in [`screens`]; what the run
//! learns goes into the [`Report`].

#[cfg(target_os = "ios")]
mod ios;
mod report;
pub mod screens;

use std::sync::{Arc, Mutex};

use corundum_gui::{Shell, egui};

pub use report::{Report, SharedReport};
pub use screens::Screen;

use screens::display::DisplayScreen;

/// The app: the shell, its screens' state and the shared report.
struct SpikeApp {
    shell: Shell<Screen>,
    report: SharedReport,
    display: DisplayScreen,
}

impl SpikeApp {
    fn new(ctx: &egui::Context) -> Self {
        let mut report = Report::default();
        // Replaces egui's font set, so it comes before anything draws.
        let cjk = corundum_gui::fonts::install_system_cjk_fallback(ctx);
        report.set("fonts.cjk", cjk.as_deref().unwrap_or("none"));
        let display = DisplayScreen::new(&mut report);
        Self {
            shell: Shell::new(Screen::destinations()),
            report: Arc::new(Mutex::new(report)),
            display,
        }
    }
}

impl eframe::App for SpikeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let report = self.report.clone();
        // A panic in another thread must not take the UI down with the lock.
        let mut report = report.lock().unwrap_or_else(|e| e.into_inner());
        self.display.update(ui.ctx(), &mut report);
        let display = &mut self.display;
        self.shell.ui(ui, |ui, screen| {
            egui::CentralPanel::default().show(ui, |ui| match screen {
                Screen::Display => display.ui(ui, &mut report),
                Screen::Audio => screens::not_implemented(ui, "Audio"),
                Screen::Photos => screens::not_implemented(ui, "Photos"),
                Screen::Opus => screens::not_implemented(ui, "Opus"),
                Screen::Report => screens::report::ui(ui, &report),
            });
        });
    }
}

/// Open the spike's window and run until it closes. On iOS this is called on the main
/// thread and does not return until the app exits.
pub fn run() -> eframe::Result {
    eframe::run_native(
        "Asteria Spike",
        eframe::NativeOptions::default(),
        Box::new(|cc| Ok(Box::new(SpikeApp::new(&cc.egui_ctx)))),
    )
}

/// Put `text` on the system clipboard.
pub(crate) fn copy_report(ctx: &egui::Context, text: &str) {
    #[cfg(target_os = "ios")]
    {
        let _ = ctx;
        ios::pasteboard::set_string(text);
    }
    #[cfg(not(target_os = "ios"))]
    ctx.copy_text(text.to_owned());
}

/// The entry point `main.m` calls on the main thread. Errors and panics are logged (to
/// the unified log and the Xcode console) instead of crashing, because unwinding across the C boundary
/// would abort without saying why.
#[cfg(target_os = "ios")]
#[unsafe(no_mangle)]
pub extern "C" fn asteria_spike_main() {
    use ios::log::{install_logger, install_panic_hook, log};
    install_panic_hook();
    install_logger();
    log("asteria_spike_main: start");
    match std::panic::catch_unwind(run) {
        Ok(Ok(())) => log("asteria_spike_main: eframe returned"),
        Ok(Err(e)) => log(&format!("asteria_spike_main: run failed: {e}")),
        Err(_) => log("asteria_spike_main: run panicked"),
    }
}
