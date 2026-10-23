//! Logging that reaches the unified log and Xcode's console. `eprintln!` output does not
//! show up in `simctl` logs, so startup errors and panics go through `NSLog` as well.

use objc2_foundation::NSString;

unsafe extern "C" {
    /// Foundation's variadic logger; only ever called with a `%@` format.
    fn NSLog(format: *const NSString, ...);
}

/// Write `message` to the unified log (and stderr, as `NSLog` does on a device).
pub fn log(message: &str) {
    let format = NSString::from_str("%@");
    let message = NSString::from_str(message);
    // SAFETY: both arguments are live NSString objects and the format has one `%@`.
    unsafe { NSLog(&*format, &*message as *const NSString) };
}

/// Forward panic messages to [`log`], keeping the default hook's behaviour too.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("asteria_spike: panic: {info}"));
        default(info);
    }));
}

/// Targets whose debug records are worth the volume while chasing startup problems.
const DEBUG_TARGETS: [&str; 4] = ["eframe", "egui", "winit", "wgpu"];

/// A `log::Log` that writes records to [`log`], so errors that eframe, wgpu and winit
/// report through the `log` crate (and tracing, via its `log` feature) are visible.
struct NsLogger;

impl NsLogger {
    fn wanted(metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
            || (metadata.level() == log::Level::Debug
                && DEBUG_TARGETS
                    .iter()
                    .any(|prefix| metadata.target().starts_with(prefix)))
    }
}

impl log::Log for NsLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        Self::wanted(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            log(&format!(
                "[{} {}] {}",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

/// Route the `log` crate to NSLog. Call once, before `run()`.
pub fn install_logger() {
    static LOGGER: NsLogger = NsLogger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
}
