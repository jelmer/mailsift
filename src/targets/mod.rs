pub mod local_events;
pub mod sink;

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use icalendar::{Calendar, CalendarComponent, Component};

pub use sink::FileOutcome;

/// An event ready to be filed: its UID and a single-VEVENT iCalendar body.
pub struct SingleEvent {
    pub uid: String,
    pub body: String,
}

/// Trait implemented by anything that can accept a stream of single
/// events. Implementations encapsulate their own dedup / overwrite rules.
pub trait EventSink {
    fn file(&self, event: &SingleEvent) -> anyhow::Result<FileOutcome>;
}

/// Configuration-derived event sink. Built once at startup.
pub enum EventSinkKind {
    LocalDir(PathBuf),
}

impl EventSink for EventSinkKind {
    fn file(&self, event: &SingleEvent) -> anyhow::Result<FileOutcome> {
        match self {
            EventSinkKind::LocalDir(dir) => local_events::file_single(event, dir),
        }
    }
}

/// Parse a .ics body and split it into single-VEVENT calendars.
pub fn split_calendar(body: &str) -> Result<Vec<SingleEvent>> {
    let calendar: Calendar = body
        .parse()
        .map_err(|e| anyhow!("parsing calendar body: {e}"))?;

    let mut out = Vec::new();
    for component in calendar.components.iter() {
        let event = match component {
            CalendarComponent::Event(ev) => ev,
            _ => continue,
        };
        let uid = match event.get_uid() {
            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => continue,
        };
        let mut single = Calendar::new();
        single.push(event.clone());
        out.push(SingleEvent {
            uid,
            body: single.to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICS_TWO: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:a@example.org\r\n\
DTSTAMP:20260101T120000Z\r\n\
DTSTART:20260201T100000Z\r\n\
DTEND:20260201T110000Z\r\n\
SUMMARY:A\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:b@example.org\r\n\
DTSTAMP:20260101T120000Z\r\n\
DTSTART:20260202T100000Z\r\n\
DTEND:20260202T110000Z\r\n\
SUMMARY:B\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn split_yields_one_per_vevent() {
        let events = split_calendar(ICS_TWO).expect("parse");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].uid, "a@example.org");
        assert_eq!(events[1].uid, "b@example.org");
    }

    #[test]
    fn split_skips_events_without_uid() {
        let body = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
BEGIN:VEVENT\r\n\
DTSTAMP:20260101T120000Z\r\n\
DTSTART:20260201T100000Z\r\n\
SUMMARY:no uid\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = split_calendar(body).expect("parse");
        assert_eq!(events.len(), 0);
    }
}
