#[cfg(not(any(target_os = "ios", target_os = "android")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    asteria_photos::desktop::run()
}

/// Phones start the app from their own entry point, not this binary.
#[cfg(any(target_os = "ios", target_os = "android"))]
fn main() {}
