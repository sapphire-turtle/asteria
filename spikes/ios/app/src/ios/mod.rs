//! iOS-only code. Everything that touches UIKit, AVFoundation or MediaPlayer lives under
//! this module.

pub mod audio_session;
pub mod log;
pub mod now_playing;
pub mod pasteboard;

/// Run `f`, logging instead of unwinding if it panics. Objective-C blocks call back into
/// Rust through a C boundary, where a panic must not escape.
pub(crate) fn guarded<R>(context: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        self::log::log(&format!("asteria_spike: {context} panicked"));
        fallback
    })
}
