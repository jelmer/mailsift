pub mod bills;
pub mod caldav;
#[cfg(test)]
mod fake_dav;
pub mod firefly;
pub mod http_auth;
pub mod http_client;
pub mod json_target;
pub mod karrio;
pub mod local_events;
pub mod mail_forward;
pub mod parcels;
pub mod receipts;
pub mod reservations;
pub mod seventeentrack;
pub mod sink;
pub mod subscriptions;
pub mod tickets;
pub mod trackers;
pub mod webdav;

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use icalendar::{Calendar, CalendarComponent, Component};
use tracing::warn;

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
    /// The VEVENT's `DTSTAMP` as its source supplied it: when the
    /// organiser created this take on the event. Event sinks compare
    /// it before replacing an event, so the newest take wins whatever
    /// order messages are processed in.
    ///
    /// `None` for an event whose source left it out or that we
    /// rendered ourselves. The `DTSTAMP` in `body` is then only the
    /// time of rendering; [`SingleEvent::with_dtstamp`] replaces it
    /// with the date of the message the event arrived in.
    pub dtstamp: Option<DateTime<Utc>>,
}

impl SingleEvent {
    /// A copy of this event with `dtstamp` as its `DTSTAMP`.
    pub fn with_dtstamp(&self, dtstamp: DateTime<Utc>) -> Result<SingleEvent> {
        let mut calendar: Calendar = self
            .body
            .parse()
            .map_err(|e| anyhow!("parsing calendar body: {e}"))?;
        for component in &mut calendar.components {
            if let CalendarComponent::Event(event) = component {
                event.timestamp(dtstamp);
            }
        }
        Ok(SingleEvent {
            uid: self.uid.clone(),
            body: calendar.to_string(),
            method: self.method.clone(),
            dtstamp: Some(dtstamp),
        })
    }
}

/// The newest `DTSTAMP` among the VEVENTs of an iCalendar body, or
/// `None` if none carries a usable one.
pub fn dtstamp_in(body: &str) -> Result<Option<DateTime<Utc>>> {
    let calendar: Calendar = body
        .parse()
        .map_err(|e| anyhow!("parsing calendar body: {e}"))?;
    Ok(calendar
        .components
        .iter()
        .filter_map(|component| match component {
            CalendarComponent::Event(event) => event.get_timestamp(),
            _ => None,
        })
        .max())
}

/// Whether the event body `existing`, found at `label`, is a newer
/// take on its event than one with a `DTSTAMP` of `incoming`.
///
/// An event we can't parse has no date to order by, so it never wins:
/// replacing a corrupt event is better than keeping it forever.
pub fn event_is_newer(existing: &[u8], label: &str, incoming: Option<DateTime<Utc>>) -> bool {
    let existing = std::str::from_utf8(existing)
        .context("decoding calendar body")
        .and_then(dtstamp_in)
        .unwrap_or_else(|e| {
            warn!(target = %label, error = format!("{e:#}"), "existing event is unreadable; replacing");
            None
        });
    sink::supersedes(existing, incoming)
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
            dtstamp: event.get_timestamp(),
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

    const ICS_NO_DTSTAMP: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:undated-1@example.org\r\n\
DTSTART:20260201T100000Z\r\n\
SUMMARY:Undated event\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn at(rfc3339: &str) -> DateTime<Utc> {
        rfc3339.parse().unwrap()
    }

    #[test]
    fn split_reports_the_source_dtstamp() {
        let events = split_calendar(ICS_REQUEST).expect("parse");
        assert_eq!(events[0].dtstamp, Some(at("2026-01-01T12:00:00Z")));
        assert_eq!(
            dtstamp_in(&events[0].body).unwrap(),
            Some(at("2026-01-01T12:00:00Z"))
        );
    }

    #[test]
    fn split_reports_no_dtstamp_when_the_source_has_none() {
        let events = split_calendar(ICS_NO_DTSTAMP).expect("parse");
        assert_eq!(events[0].dtstamp, None);
    }

    #[test]
    fn with_dtstamp_dates_an_undated_event() {
        let event = split_calendar(ICS_NO_DTSTAMP).expect("parse").remove(0);
        let dated = event.with_dtstamp(at("2026-01-28T10:00:00Z")).unwrap();
        assert_eq!(dated.dtstamp, Some(at("2026-01-28T10:00:00Z")));
        assert_eq!(
            dtstamp_in(&dated.body).unwrap(),
            Some(at("2026-01-28T10:00:00Z"))
        );
        assert_eq!(dated.uid, event.uid);
        assert_eq!(dated.method, event.method);
    }

    #[test]
    fn with_dtstamp_only_changes_the_dtstamp() {
        let event = split_calendar(ICS_REQUEST).expect("parse").remove(0);
        let dated = event.with_dtstamp(at("2026-01-28T10:00:00Z")).unwrap();
        let expected: Vec<String> = event
            .body
            .lines()
            .map(|l| l.replace("DTSTAMP:20260101T120000Z", "DTSTAMP:20260128T100000Z"))
            .collect();
        assert_eq!(dated.body.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn event_is_newer_compares_dtstamps() {
        let existing = ICS_NO_METHOD.as_bytes();
        assert!(event_is_newer(
            existing,
            "x",
            Some(at("2025-12-31T12:00:00Z"))
        ));
        assert!(!event_is_newer(
            existing,
            "x",
            Some(at("2026-01-01T12:00:00Z"))
        ));
        assert!(!event_is_newer(existing, "x", None));
    }

    #[test]
    fn event_is_newer_never_keeps_an_unreadable_event() {
        let incoming = Some(at("2025-10-28T10:00:00Z"));
        assert!(!event_is_newer(b"not a calendar", "x", incoming));
        assert!(!event_is_newer(&[0xff, 0xfe], "x", incoming));
    }

    #[test]
    fn split_without_method_yields_none() {
        let events = split_calendar(ICS_NO_METHOD).expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "plain-1@example.org");
        assert_eq!(events[0].method, None);
    }
}
