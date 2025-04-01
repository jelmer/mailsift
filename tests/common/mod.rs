//! Helpers shared between integration tests.

use std::path::Path;

/// Read an .ics file and return its body with the wall-clock `DTSTAMP`
/// line stripped, so tests can assert against a fixed expected body.
pub fn read_event_stable(path: &Path) -> String {
    let body =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    body.lines()
        .filter(|l| !l.starts_with("DTSTAMP:"))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}
