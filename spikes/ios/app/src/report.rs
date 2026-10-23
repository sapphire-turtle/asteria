//! The spike's report: every measurement and checklist answer as `key: value` lines, so the
//! whole run can be copied off the device in one piece.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::{Arc, Mutex};

/// The report, shared between the screens and anything that fills it in.
pub type SharedReport = Arc<Mutex<Report>>;

/// Measurements and checklist answers, keyed by dotted names such as `display.ppp`.
#[derive(Debug, Default)]
pub struct Report {
    entries: BTreeMap<String, String>,
}

impl Report {
    /// Record `value` under `key`, replacing any earlier value.
    pub fn set(&mut self, key: &str, value: impl Display) {
        self.entries.insert(key.to_owned(), value.to_string());
    }

    /// The report as plain text: a header line naming the build, then one `key: value`
    /// line per entry in key order.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "asteria-ios-spike {} ({}/{})\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        );
        for (key, value) in &self.entries {
            text.push_str(&format!("{key}: {value}\n"));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_text_has_header_and_sorted_lines() {
        let mut report = Report::default();
        report.set("b", 1);
        report.set("a", "x");
        let text = report.to_text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("asteria-ios-spike "));
        assert_eq!(lines[1], "a: x");
        assert_eq!(lines[2], "b: 1");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn setting_a_key_twice_keeps_the_last_value() {
        let mut report = Report::default();
        report.set("k", "first");
        report.set("k", "second");
        let text = report.to_text();
        assert!(text.contains("k: second"));
        assert!(!text.contains("first"));
    }
}
