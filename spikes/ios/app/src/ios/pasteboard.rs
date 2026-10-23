//! The system clipboard, through UIKit.
//!
//! egui's own clipboard support (`Context::copy_text`) goes through winit's platform
//! output, which does not reach `UIPasteboard` on iOS, so the report's copy button calls
//! this instead.

use objc2_foundation::NSString;
use objc2_ui_kit::UIPasteboard;

/// Put `text` on the general pasteboard. Call it from the UI (main) thread.
pub fn set_string(text: &str) {
    let string = NSString::from_str(text);
    let pasteboard = UIPasteboard::generalPasteboard();
    // SAFETY: `setString:` is documented as not thread-safe; the only caller is the egui
    // update on the main thread, and `string` is a valid NSString that outlives the call.
    unsafe { pasteboard.setString(Some(&string)) };
}
