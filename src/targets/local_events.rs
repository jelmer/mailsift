//! Local-directory target for `event` artifacts.
//!
//! Each event is filed as `<UID>.ics` under the configured directory.
//! An existing file is overwritten, same as a CalDAV PUT by UID,
//! unless it is a newer take on the event (per `DTSTAMP`).

use std::path::Path;

use anyhow::Result;

use super::sink::{log_file_outcome, log_kept, read_if_exists, sanitize_uid, write_atomic};
use super::{FileOutcome, SingleEvent, event_is_newer};

pub fn file_single(event: &SingleEvent, dir: &Path) -> Result<FileOutcome> {
    let target = dir.join(sanitize_uid(&event.uid)).with_extension("ics");
    let label = target.display().to_string();
    let existing = read_if_exists(&target)?;
    if let Some(existing) = &existing
        && event_is_newer(existing, &label, event.dtstamp)
    {
        return Ok(log_kept(label, "event"));
    }
    write_atomic(&target, event.body.as_bytes())?;
    Ok(log_file_outcome(&target, existing.is_some(), "event"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::split_calendar;

    fn event(summary: &str, dtstamp: &str) -> SingleEvent {
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nBEGIN:VEVENT\r\n\
UID:train-1@mailsift\r\nDTSTART:20260201T100000Z\r\n\
SUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        split_calendar(&ics)
            .unwrap()
            .remove(0)
            .with_dtstamp(dtstamp.parse().unwrap())
            .unwrap()
    }

    #[test]
    fn older_take_does_not_replace_event() {
        let tmp = tempfile::tempdir().unwrap();
        let rebooked = event("Rebooked", "2026-01-28T10:00:00Z");
        let original = event("Original", "2026-01-20T10:00:00Z");

        let first = file_single(&rebooked, tmp.path()).unwrap();
        assert!(matches!(first, FileOutcome::Created(_)));
        let second = file_single(&original, tmp.path()).unwrap();
        assert!(matches!(second, FileOutcome::Kept(_)));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("train-1@mailsift.ics")).unwrap(),
            rebooked.body
        );
    }

    #[test]
    fn newer_take_replaces_event() {
        let tmp = tempfile::tempdir().unwrap();
        let original = event("Original", "2026-01-20T10:00:00Z");
        let rebooked = event("Rebooked", "2026-01-28T10:00:00Z");

        file_single(&original, tmp.path()).unwrap();
        let second = file_single(&rebooked, tmp.path()).unwrap();
        assert!(matches!(second, FileOutcome::Updated(_)));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("train-1@mailsift.ics")).unwrap(),
            rebooked.body
        );
    }
}
