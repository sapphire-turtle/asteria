//! The rule for paths under `Inbox/`, shared by the server and the clients.

/// Characters no path may contain: separators other than `/`, and what Windows forbids.
const FORBIDDEN: [char; 8] = ['\\', ':', '<', '>', '"', '|', '?', '*'];

/// Windows device names, reserved with any extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// `/`-separated and relative. Every element is non-empty, does not start with `.`, does
/// not end in `.` or a space, and is not a Windows device name; no [`FORBIDDEN`] or control
/// character anywhere.
pub fn validate_inbox_path(path: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("path {path:?}: {why}"));
    if path.is_empty() {
        return invalid("empty");
    }
    if path
        .chars()
        .any(|c| FORBIDDEN.contains(&c) || c.is_control())
    {
        return invalid("contains a character not allowed in file names");
    }
    for element in path.split('/') {
        if element.is_empty() {
            return invalid("empty element");
        }
        if element.starts_with('.') {
            return invalid("element starts with '.'");
        }
        if element.ends_with(['.', ' ']) {
            return invalid("element ends with '.' or a space");
        }
        let stem = element.split('.').next().unwrap_or(element);
        if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
            return invalid("element is a reserved device name");
        }
    }
    Ok(())
}
