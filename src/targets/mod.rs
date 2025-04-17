pub mod bills;
pub mod caldav;
pub mod firefly;
pub mod http_auth;
pub mod http_client;
pub mod json_target;
pub mod local_events;
pub mod sink;
pub mod webdav;

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use icalendar::{Calendar, CalendarComponent, Component};

pub use sink::FileOutcome;

/// An event ready to be filed: its UID, a single-VEVENT iCalendar body,
/// and the iTIP METHOD copied from the enclosing calendar (if any).
///
/// The METHOD distinguishes a plain calendar entry (none, or `PUBLISH`)
/// from an iMIP scheduling message (`REQUEST`, `REPLY`, `CANCEL`, ...).
/// CalDAV targets use it to decide whether to file to the schedule
/// inbox or to the default calendar; other sinks ignore it.
pub struct SingleEvent {
    pub uid: String,
    pub body: String,
    pub method: Option<String>,
}

/// Trait implemented by anything that can accept a stream of single
/// events. Implementations encapsulate their own dedup / overwrite rules.
pub trait EventSink {
    fn file(&self, event: &SingleEvent) -> anyhow::Result<FileOutcome>;
}

/// Configuration-derived event sink. Built once at startup.
pub enum EventSinkKind {
    LocalDir(PathBuf),
    Caldav(caldav::CaldavSink),
}

impl EventSink for EventSinkKind {
    fn file(&self, event: &SingleEvent) -> anyhow::Result<FileOutcome> {
        match self {
            EventSinkKind::LocalDir(dir) => local_events::file_single(event, dir),
            EventSinkKind::Caldav(sink) => sink.file(event),
        }
    }
}

/// Parse a .ics body and split it into single-VEVENT calendars. Each
/// resulting calendar inherits the parent's `METHOD` so downstream sinks
/// can tell iMIP scheduling messages apart from plain events.
pub fn split_calendar(body: &str) -> Result<Vec<SingleEvent>> {
    let calendar: Calendar = body
        .parse()
        .map_err(|e| anyhow!("parsing calendar body: {e}"))?;

    let method = calendar
        .property_value("METHOD")
        .map(|m| m.trim().to_ascii_uppercase())
        .filter(|m| !m.is_empty());

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
        if let Some(m) = method.as_deref() {
            single.append_property(("METHOD", m));
        }
        out.push(SingleEvent {
            uid,
            body: single.to_string(),
            method: method.clone(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICS_REQUEST: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:invite-1@example.org\r\n\
DTSTAMP:20260101T120000Z\r\n\
DTSTART:20260201T100000Z\r\n\
DTEND:20260201T110000Z\r\n\
SUMMARY:Lunch\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    const ICS_NO_METHOD: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:plain-1@example.org\r\n\
DTSTAMP:20260101T120000Z\r\n\
DTSTART:20260201T100000Z\r\n\
DTEND:20260201T110000Z\r\n\
SUMMARY:Plain event\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn split_preserves_method_request() {
        let events = split_calendar(ICS_REQUEST).expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "invite-1@example.org");
        assert_eq!(events[0].method.as_deref(), Some("REQUEST"));
    }

    #[test]
    fn split_without_method_yields_none() {
        let events = split_calendar(ICS_NO_METHOD).expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "plain-1@example.org");
        assert_eq!(events[0].method, None);
    }
}
