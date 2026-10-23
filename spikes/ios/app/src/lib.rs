//! A spike app for iOS: what egui/eframe, corundum-gui and the platform do on a real
//! device (display, touch, audio, photos). The screens live in [`screens`]; what the run
//! learns goes into the [`Report`].

pub mod audio;
#[cfg(target_os = "ios")]
mod ios;
mod report;
pub mod screens;

use std::sync::{Arc, Mutex};

use corundum_gui::{Shell, egui};

pub use report::{Report, SharedReport};
pub use screens::Screen;

#[cfg(target_os = "ios")]
use audio::Source;
use audio::{Engine, PlayState};
use screens::audio::AudioScreen;
use screens::display::DisplayScreen;

/// The app: the shell, its screens' state and the shared report.
struct SpikeApp {
    shell: Shell<Screen>,
    report: SharedReport,
    display: DisplayScreen,
    audio: AudioScreen,
    engine: Engine,
    /// Notification observers and remote-command targets, alive as long as the app.
    #[cfg(target_os = "ios")]
    _audio_hooks: (
        ios::audio_session::Observers,
        ios::now_playing::RemoteTargets,
    ),
}

impl SpikeApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        let mut report = Report::default();
        let gpu = cc.wgpu_render_state.as_ref().map(|state| {
            let info = state.adapter.get_info();
            format!("{} ({:?})", info.name, info.backend)
        });
        report.set("gpu.adapter", gpu.as_deref().unwrap_or("none"));
        // Replaces egui's font set, so it comes before anything draws.
        let cjk = corundum_gui::fonts::install_system_cjk_fallback(ctx);
        report.set("fonts.cjk", cjk.as_deref().unwrap_or("none"));
        let display = DisplayScreen::new(&mut report);
        let audio = AudioScreen::new(&mut report);
        // The session must be in the playback category before the first stream exists.
        #[cfg(target_os = "ios")]
        report.set(
            "audio.session",
            match ios::audio_session::activate_playback() {
                Ok(()) => "playback, active".to_owned(),
                Err(e) => e,
            },
        );
        let report = Arc::new(Mutex::new(report));
        let engine = Engine::start(report.clone(), on_playback_change(ctx.clone()));
        Self {
            shell: Shell::new(Screen::destinations()),
            #[cfg(target_os = "ios")]
            _audio_hooks: (
                ios::audio_session::observe(engine.sender(), report.clone()),
                ios::now_playing::register(engine.sender(), report.clone()),
            ),
            report,
            display,
            audio,
            engine,
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
        let (audio, engine) = (&mut self.audio, &self.engine);
        self.shell.ui(ui, |ui, screen| {
            egui::CentralPanel::default().show(ui, |ui| match screen {
                Screen::Display => display.ui(ui, &mut report),
                Screen::Audio => audio.ui(ui, engine, &mut report),
                Screen::Photos => screens::not_implemented(ui, "Photos"),
                Screen::Opus => screens::not_implemented(ui, "Opus"),
                Screen::Report => screens::report::ui(ui, &report),
            });
        });
    }
}

/// What happens when playback changes (and every few seconds while it plays), on the
/// audio engine's thread: redraw the UI and, on iOS, update the lock screen.
fn on_playback_change(ctx: egui::Context) -> impl FnMut(PlayState, f64) + Send + 'static {
    move |state, elapsed_secs| {
        ctx.request_repaint();
        #[cfg(target_os = "ios")]
        ios::now_playing::update(
            state.source().map_or("Stopped", Source::title),
            matches!(state, PlayState::Playing(_)),
            elapsed_secs,
        );
        #[cfg(not(target_os = "ios"))]
        let _ = (state, elapsed_secs);
    }
}

/// Open the spike's window and run until it closes. On iOS this is called on the main
/// thread and does not return until the app exits.
pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    eframe::run_native(
        "Asteria Spike",
        options,
        Box::new(|cc| Ok(Box::new(SpikeApp::new(cc)))),
    )
}

/// egui's wgpu setup, with the requested device limits clamped to what the adapter
/// reports. wgpu's defaults exceed some adapters (the iOS simulator's Metal one), and
/// requesting more than the adapter has makes device creation fail.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::WgpuSetup;

    let mut options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_setup {
        let egui_default = Arc::clone(&setup.device_descriptor);
        setup.device_descriptor = Arc::new(move |adapter| {
            let mut descriptor = egui_default(adapter);
            let supported = adapter.limits();
            descriptor.required_limits =
                descriptor.required_limits.or_worse_values_from(&supported);
            log::info!("wgpu adapter: {:?}", adapter.get_info());
            log::info!("wgpu requested limits: {:?}", descriptor.required_limits);
            descriptor
        });
    }
    options
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
