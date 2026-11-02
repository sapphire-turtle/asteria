//! `asteria-photos-console`, the Windows console shim the package installs as `asteria-photos.exe`:
//! the GUI binary has no console, so this one, on `PATH`, starts it with the terminal's
//! arguments and streams and returns its exit code.

#[cfg(windows)]
fn main() {
    corundum_framework::package::console::forward("asteria-photos-desktop.exe")
}

/// Elsewhere the GUI binary answers in a terminal by itself.
#[cfg(not(windows))]
fn main() {
    eprintln!("asteria-photos-console is for Windows only: run asteria-photos-desktop");
    std::process::exit(2);
}
