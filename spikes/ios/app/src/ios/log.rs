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
