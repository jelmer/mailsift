//! Per-artifact routing: each successful extractor run hands its
//! artifacts here, and the router files them to the appropriate sink
//! (event sink, bills dir, parcels dir, …). The routing decisions
//! (which kind goes where, what fallback year to use for tickets, how
//! to render a multi-artifact rollup line) all live in this module
//! so `pipeline/mod.rs` can stay focused on MIME parsing and
//! extractor dispatch.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use tracing::{debug, warn};

use crate::artifacts::{Artifact, Kind};
use crate::reservation;
use crate::seen::{self, Store as SeenStore};
use crate::targets::{
    EventSink, EventSinkKind, FileOutcome, SingleEvent, bills, parcels, receipts, reservations,
    split_calendar, subscriptions, tickets,
};

pub(super) const KIND_EVENT: usize = 0;
pub(super) const KIND_BILL: usize = 1;
pub(super) const KIND_PARCEL: usize = 2;
pub(super) const KIND_RECEIPT: usize = 3;
pub(super) const KIND_TICKET: usize = 4;
pub(super) const KIND_SUBSCRIPTION: usize = 5;
pub(super) const KIND_RESERVATION: usize = 6;

/// Per-`pipeline::run` tally of successful artifact filings. Used to
/// emit one compact INFO line per source message instead of one per
/// artifact.
#[derive(Default)]
pub(super) struct Summary {
    /// `extractor name -> per-kind counts` (indexed by the `KIND_*`
    /// constants). `BTreeMap` so the rendered string is stable across
    /// invocations.
    counts: BTreeMap<String, [u32; 7]>,
    /// When set, the filer functions skip the actual target write
    /// (disk / CalDAV / SMTP / Firefly / trackers) but still bump the
    /// summary as if the write had succeeded. Reports what `--dry-run`
    /// would do.
    pub(super) dry_run: bool,
}

impl Summary {
    pub(super) fn new(dry_run: bool) -> Self {
        Self {
            counts: BTreeMap::new(),
            dry_run,
        }
    }

    pub(super) fn bump(&mut self, extractor: &str, kind_index: usize) {
        // Avoid allocating a new `String` on every bump — only allocate
        // when this is the first sighting of the extractor. `BTreeMap`
        // has no `entry_ref`, so we do the two-lookup dance manually.
        if let Some(counts) = self.counts.get_mut(extractor) {
            counts[kind_index] += 1;
        } else {
            let mut counts = [0u32; 7];
            counts[kind_index] = 1;
            self.counts.insert(extractor.to_string(), counts);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Render as e.g. `easyjet=2 events, ns=1 bill + 1 event`.
    pub(super) fn render(&self) -> String {
        let mut out = String::new();
        for (i, (extractor, counts)) in self.counts.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{extractor}=");
            let mut wrote_kind = false;
            for (count, singular, plural) in [
                (counts[KIND_EVENT], "event", "events"),
                (counts[KIND_BILL], "bill", "bills"),
                (counts[KIND_PARCEL], "parcel", "parcels"),
                (counts[KIND_RECEIPT], "receipt", "receipts"),
                (counts[KIND_TICKET], "ticket", "tickets"),
                (counts[KIND_SUBSCRIPTION], "subscription", "subscriptions"),
                (counts[KIND_RESERVATION], "reservation", "reservations"),
            ] {
                if count == 0 {
                    continue;
                }
                if wrote_kind {
                    out.push_str(" + ");
                }
                let label = if count == 1 { singular } else { plural };
                let _ = write!(out, "{count} {label}");
                wrote_kind = true;
            }
        }
        out
    }
}

pub(super) fn file_event_artifact(
    extractor: &str,
    artifact: &Artifact,
    event_sink: &EventSinkKind,
    seen: Option<&SeenStore>,
    summary: &mut Summary,
) {
    let body = match fs::read_to_string(&artifact.path) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to read event body"
            );
            return;
        }
    };

    let singles = match split_calendar(&body) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to parse event body"
            );
            return;
        }
    };

    if singles.is_empty() {
        warn!(
            extractor,
            path = %artifact.path.display(),
            "no usable VEVENT (missing UID?)"
        );
        return;
    }

    for event in &singles {
        file_single(extractor, event, event_sink, seen, summary);
    }
}

/// File the raw `.reservation.json` payload under `reservations_dir`.
///
/// Separate from the calendar conversion below: the VEVENT keeps only
/// what iCalendar can express, so the booking reference, passenger and
/// price survive only in the JSON. A reservation we can't render as an
/// event is still worth archiving, and vice versa.
pub(super) fn file_reservation_json(
    extractor: &str,
    artifact: &Artifact,
    reservations_dir: &Path,
    received_at_epoch: Option<i64>,
    summary: &mut Summary,
) {
    if summary.dry_run {
        summary.bump(extractor, KIND_RESERVATION);
        return;
    }
    match reservations::file_reservation(&artifact.path, reservations_dir, received_at_epoch) {
        // One bump per record written, matching the event side, where
        // a multi-leg itinerary counts as several events.
        Ok(outcomes) => {
            for _ in outcomes {
                summary.bump(extractor, KIND_RESERVATION);
            }
        }
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to file reservation"
            );
        }
    }
}

pub(super) fn file_reservation_artifact(
    extractor: &str,
    artifact: &Artifact,
    event_sink: &EventSinkKind,
    seen: Option<&SeenStore>,
    summary: &mut Summary,
) {
    let singles = match reservation::convert_file(&artifact.path) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to convert reservation"
            );
            return;
        }
    };
    if singles.is_empty() {
        debug!(
            extractor,
            path = %artifact.path.display(),
            "reservation type not supported; ignoring"
        );
        return;
    }
    for single in &singles {
        file_single(extractor, single, event_sink, seen, summary);
    }
}

pub(super) fn file_bill_artifact(
    extractor: &str,
    artifact: &Artifact,
    bills_dir: &Path,
    firefly: Option<&crate::targets::firefly::FireflySink>,
    received_at_epoch: Option<i64>,
    summary: &mut Summary,
) {
    if summary.dry_run {
        summary.bump(extractor, KIND_BILL);
        return;
    }
    match bills::file_bill(&artifact.path, bills_dir, firefly, received_at_epoch) {
        Ok(FileOutcome::Created(_) | FileOutcome::Updated(_)) => {
            summary.bump(extractor, KIND_BILL);
        }
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to file bill"
            );
        }
    }
}

pub(super) fn file_parcel_artifact(
    extractor: &str,
    artifact: &Artifact,
    parcels_dir: &Path,
    trackers: Option<&crate::targets::trackers::Trackers>,
    received_at_epoch: Option<i64>,
    summary: &mut Summary,
) {
    if summary.dry_run {
        summary.bump(extractor, KIND_PARCEL);
        return;
    }
    match parcels::file_parcel(&artifact.path, parcels_dir, trackers, received_at_epoch) {
        Ok(FileOutcome::Created(_) | FileOutcome::Updated(_)) => {
            summary.bump(extractor, KIND_PARCEL);
        }
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to file parcel"
            );
        }
    }
}

fn file_single(
    extractor: &str,
    event: &SingleEvent,
    event_sink: &EventSinkKind,
    seen: Option<&SeenStore>,
    summary: &mut Summary,
) {
    if summary.dry_run {
        summary.bump(extractor, KIND_EVENT);
        return;
    }
    // Skip the network round-trip when (a) we have a seen.db, (b) the
    // sink is CalDAV (local-dir rewrites are cheap, no point gating),
    // and (c) we've already PUT this exact body for this UID. Local
    // sinks fall through to the always-rewrite path below.
    let hash = seen.and_then(|_| {
        matches!(event_sink, EventSinkKind::Caldav(_)).then(|| seen::hash(event.body.as_bytes()))
    });
    if let (Some(store), Some(h)) = (seen, hash.as_deref())
        && store.is_seen(seen::Kind::Event, &event.uid, h)
    {
        debug!(extractor, uid = %event.uid, "event already filed at this hash; skipping CalDAV PUT");
        // Count toward the summary anyway; the user-visible "filed N
        // events" line should reflect what arrived, not the network
        // optimisation underneath.
        summary.bump(extractor, KIND_EVENT);
        return;
    }
    match event_sink.file(event) {
        Ok(FileOutcome::Created(_) | FileOutcome::Updated(_)) => {
            summary.bump(extractor, KIND_EVENT);
            if let (Some(store), Some(h)) = (seen, hash.as_deref()) {
                store.mark(seen::Kind::Event, &event.uid, h);
            }
        }
        Err(e) => {
            warn!(
                extractor,
                uid = %event.uid,
                error = format!("{e:#}"),
                "failed to file event"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn file_receipt_artifact(
    extractor: &str,
    artifact: &Artifact,
    raw_message: &[u8],
    sink: &receipts::ReceiptSink,
    received_at_epoch: Option<i64>,
    summary: &mut Summary,
    recorder: &crate::stats::Recorder,
    now_ts: i64,
) {
    if summary.dry_run {
        summary.bump(extractor, KIND_RECEIPT);
        return;
    }
    match sink.file_receipt(&artifact.path, raw_message, received_at_epoch) {
        Ok(FileOutcome::Created(_) | FileOutcome::Updated(_)) => {
            summary.bump(extractor, KIND_RECEIPT);
        }
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to file receipt"
            );
            // The forward sink is the only receipt variant that talks
            // to the network (SMTP or a sendmail pipe). Its failures
            // are the ones a human might want to see on the dashboard,
            // so we record them under a synthetic extractor name.
            if matches!(sink, receipts::ReceiptSink::Forward(_)) {
                recorder.record(&crate::stats::Event {
                    ts: now_ts,
                    extractor: RECEIPTS_FORWARD_SINK_NAME.to_string(),
                    outcome: crate::stats::Outcome::Failed,
                    duration_ms: None,
                    from_domain: None,
                    error: Some(format!("{e:#}")),
                });
            }
        }
    }
}

/// Synthetic "extractor" identifier used for sink-side receipts-forward
/// failures so they show up in the aggregated stats alongside real
/// extractor runs. The angle-brackets keep the name from ever
/// colliding with a real extractor slug.
pub(super) const RECEIPTS_FORWARD_SINK_NAME: &str = "<sink:receipts-forward>";

pub(super) fn file_subscription_artifact(
    extractor: &str,
    artifact: &Artifact,
    subscriptions_dir: &Path,
    received_at_epoch: Option<i64>,
    summary: &mut Summary,
) {
    if summary.dry_run {
        summary.bump(extractor, KIND_SUBSCRIPTION);
        return;
    }
    match subscriptions::file_subscription(&artifact.path, subscriptions_dir, received_at_epoch) {
        Ok(FileOutcome::Created(_) | FileOutcome::Updated(_)) => {
            summary.bump(extractor, KIND_SUBSCRIPTION);
        }
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to file subscription"
            );
        }
    }
}

pub(super) fn file_ticket_artifact(
    extractor: &str,
    artifact: &Artifact,
    year: i32,
    sink: &tickets::TicketSink,
    meta: &tickets::TicketMeta,
    received_at_epoch: Option<i64>,
    summary: &mut Summary,
) {
    if summary.dry_run {
        summary.bump(extractor, KIND_TICKET);
        return;
    }
    match sink.file_ticket(
        &artifact.path,
        &artifact.slug,
        &artifact.ext,
        year,
        meta,
        received_at_epoch,
    ) {
        Ok(FileOutcome::Created(_) | FileOutcome::Updated(_)) => {
            summary.bump(extractor, KIND_TICKET);
        }
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to file ticket"
            );
        }
    }
}

/// Pull a year out of the leading `YYYY` of a string. Returns `None`
/// if the first four chars don't parse as a four-digit integer in a
/// sensible range.
fn year_from_iso_prefix(s: &str) -> Option<i32> {
    let prefix = s.trim().get(..4)?;
    let y: i32 = prefix.parse().ok()?;
    (1970..=9999).contains(&y).then_some(y)
}

/// Pull the earliest start year we can find from this run's sibling
/// `.event.ics` / `.reservation.json` artifacts. Returns `None` if no
/// sibling is parseable.
pub(super) fn earliest_sibling_year(artifacts: &[Artifact]) -> Option<i32> {
    let mut years: Vec<i32> = Vec::new();
    for a in artifacts {
        match a.kind {
            Kind::Event => {
                if let Some(y) = year_from_event_artifact(&a.path) {
                    years.push(y);
                }
            }
            Kind::Reservation => {
                if let Some(y) = year_from_reservation_artifact(&a.path) {
                    years.push(y);
                }
            }
            _ => {}
        }
    }
    years.into_iter().min()
}

/// Read a `.event.ics` file and return the year of its earliest VEVENT
/// DTSTART.
fn year_from_event_artifact(path: &Path) -> Option<i32> {
    use chrono::Datelike;
    use icalendar::Component;
    let body = fs::read_to_string(path).ok()?;
    let calendar: icalendar::Calendar = body.parse().ok()?;
    let mut years: Vec<i32> = Vec::new();
    for component in &calendar.components {
        let icalendar::CalendarComponent::Event(ev) = component else {
            continue;
        };
        if let Some(start) = ev.get_start() {
            let date: chrono::NaiveDate = start.into();
            years.push(date.year());
        }
    }
    years.into_iter().min()
}

/// Minimal struct for the year-of-earliest-date computation. We need
/// the start date from a reservation but don't want to depend on
/// `crate::reservation`'s full deserialisation succeeding; the date
/// fields here are intentionally `String` rather than parsed
/// date-times so the cheap year-prefix extraction still works against
/// inputs we can't fully understand (e.g. Tebi's `InstantLocalizable`,
/// which the full reservation deserializer handles but which the
/// year-prefix path would also accept after we strip the wrapper).
#[derive(Debug, serde::Deserialize)]
struct ReservationDates {
    #[serde(default, rename = "checkinTime")]
    checkin_time: Option<String>,
    #[serde(default, rename = "startTime")]
    start_time: Option<String>,
    #[serde(default, rename = "reservationFor")]
    reservation_for: Option<NestedDates>,
}

#[derive(Debug, serde::Deserialize)]
struct NestedDates {
    #[serde(default, rename = "departureTime")]
    departure_time: Option<String>,
    #[serde(default, rename = "startDate")]
    start_date: Option<String>,
    #[serde(default, rename = "doorTime")]
    door_time: Option<String>,
}

/// One or many reservations: schema-ld extractors sometimes emit a
/// top-level array even for a single trip.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum ReservationDatesDoc {
    Many(Vec<ReservationDates>),
    One(Box<ReservationDates>),
}

impl ReservationDatesDoc {
    fn into_vec(self) -> Vec<ReservationDates> {
        match self {
            ReservationDatesDoc::One(r) => vec![*r],
            ReservationDatesDoc::Many(rs) => rs,
        }
    }
}

/// Collect identifying fields from this run's sibling
/// `.reservation.json` artifacts for the ticket sidecar. A boarding
/// pass carries none of this itself, but the reservation filed in the
/// same extractor run does.
///
/// Takes the first reservation that yields each field; a multi-leg
/// itinerary shares its booking reference and passenger across legs,
/// so the first one is representative.
pub(super) fn sibling_ticket_meta(artifacts: &[Artifact]) -> tickets::TicketMeta {
    let mut meta = tickets::TicketMeta::default();
    for a in artifacts {
        if a.kind != Kind::Reservation {
            continue;
        }
        let Some(fields) = reservations::identifying_fields(&a.path) else {
            continue;
        };
        meta.reservation_number = meta.reservation_number.or(fields.reservation_number);
        meta.under_name = meta.under_name.or(fields.under_name);
        meta.provider = meta.provider.or(fields.provider);
    }
    meta
}

/// Read a `.reservation.json` file and return the year of its earliest
/// schema.org date field we recognise.
fn year_from_reservation_artifact(path: &Path) -> Option<i32> {
    let body = fs::read_to_string(path).ok()?;
    let parsed: ReservationDatesDoc = serde_json::from_str(&body).ok()?;
    parsed
        .into_vec()
        .into_iter()
        .flat_map(|r| {
            let nested = r.reservation_for.unwrap_or(NestedDates {
                departure_time: None,
                start_date: None,
                door_time: None,
            });
            [
                r.checkin_time,
                r.start_time,
                nested.departure_time,
                nested.start_date,
                nested.door_time,
            ]
        })
        .filter_map(|opt| opt.as_deref().and_then(year_from_iso_prefix))
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn summary_empty_renders_to_empty_string() {
        let s = Summary::default();
        assert!(s.is_empty());
        assert_eq!(s.render(), "");
    }

    #[test]
    fn summary_bump_records_one_kind() {
        let mut s = Summary::default();
        s.bump("ns", KIND_BILL);
        assert!(!s.is_empty());
        assert_eq!(s.render(), "ns=1 bill");
    }

    #[test]
    fn summary_bump_pluralises_at_two() {
        let mut s = Summary::default();
        s.bump("easyjet", KIND_EVENT);
        s.bump("easyjet", KIND_EVENT);
        assert_eq!(s.render(), "easyjet=2 events");
    }

    #[test]
    fn summary_multiple_kinds_joined_with_plus() {
        let mut s = Summary::default();
        s.bump("ns", KIND_BILL);
        s.bump("ns", KIND_EVENT);
        assert_eq!(s.render(), "ns=1 event + 1 bill");
    }

    #[test]
    fn summary_multiple_extractors_joined_with_comma() {
        let mut s = Summary::default();
        s.bump("a", KIND_EVENT);
        s.bump("b", KIND_BILL);
        assert_eq!(s.render(), "a=1 event, b=1 bill");
    }

    #[test]
    fn iso_prefix_extracts_year_from_iso_date() {
        assert_eq!(year_from_iso_prefix("2026-06-27T12:00:00Z"), Some(2026));
    }

    #[test]
    fn iso_prefix_rejects_too_short() {
        assert_eq!(year_from_iso_prefix("202"), None);
    }

    #[test]
    fn iso_prefix_rejects_non_numeric() {
        assert_eq!(year_from_iso_prefix("abcd-01-01"), None);
    }

    #[test]
    fn iso_prefix_rejects_year_out_of_range() {
        assert_eq!(year_from_iso_prefix("1969-12-31"), None);
        assert_eq!(year_from_iso_prefix("9999-01-01"), Some(9999));
    }

    #[test]
    fn iso_prefix_trims_leading_whitespace() {
        assert_eq!(year_from_iso_prefix("  2026-01-01"), Some(2026));
    }

    fn write_temp(contents: &str, ext: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(format!("test{ext}"));
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn event_artifact_year_from_single_vevent() {
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:1@example\r
DTSTART:20260627T090000Z\r
DTEND:20260627T100000Z\r
SUMMARY:t\r
END:VEVENT\r
END:VCALENDAR\r
";
        let (_d, path) = write_temp(ics, ".event.ics");
        assert_eq!(year_from_event_artifact(&path), Some(2026));
    }

    #[test]
    fn event_artifact_picks_earliest_when_multiple() {
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:1@example\r
DTSTART:20271001T090000Z\r
DTEND:20271001T100000Z\r
SUMMARY:later\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:2@example\r
DTSTART:20260101T090000Z\r
DTEND:20260101T100000Z\r
SUMMARY:earlier\r
END:VEVENT\r
END:VCALENDAR\r
";
        let (_d, path) = write_temp(ics, ".event.ics");
        assert_eq!(year_from_event_artifact(&path), Some(2026));
    }

    #[test]
    fn event_artifact_returns_none_for_unreadable_path() {
        assert_eq!(
            year_from_event_artifact(Path::new("/nonexistent/missing.ics")),
            None
        );
    }

    #[test]
    fn reservation_artifact_year_from_lodging_checkin() {
        let json = r#"{"@type":"LodgingReservation","checkinTime":"2026-08-15T15:00:00"}"#;
        let (_d, path) = write_temp(json, ".reservation.json");
        assert_eq!(year_from_reservation_artifact(&path), Some(2026));
    }

    #[test]
    fn reservation_artifact_year_from_nested_departure() {
        let json = r#"{
            "@type":"FlightReservation",
            "reservationFor":{"departureTime":"2026-09-01T10:00:00Z"}
        }"#;
        let (_d, path) = write_temp(json, ".reservation.json");
        assert_eq!(year_from_reservation_artifact(&path), Some(2026));
    }

    #[test]
    fn reservation_artifact_year_picks_earliest_from_array() {
        let json = r#"[
            {"@type":"LodgingReservation","checkinTime":"2027-01-01"},
            {"@type":"LodgingReservation","checkinTime":"2026-06-01"}
        ]"#;
        let (_d, path) = write_temp(json, ".reservation.json");
        assert_eq!(year_from_reservation_artifact(&path), Some(2026));
    }

    #[test]
    fn reservation_artifact_year_for_event_reservation() {
        let json = r#"{
            "@type":"EventReservation",
            "reservationFor":{"doorTime":"2026-12-31T19:00:00"}
        }"#;
        let (_d, path) = write_temp(json, ".reservation.json");
        assert_eq!(year_from_reservation_artifact(&path), Some(2026));
    }

    #[test]
    fn reservation_artifact_returns_none_for_bare_scalar() {
        let (_d, path) = write_temp("\"just a string\"", ".reservation.json");
        assert_eq!(year_from_reservation_artifact(&path), None);
    }

    fn artifact(kind: Kind, path: PathBuf) -> Artifact {
        Artifact {
            kind,
            path,
            slug: "t".into(),
            ext: match kind {
                Kind::Event => "ics".into(),
                Kind::Reservation => "json".into(),
                _ => "bin".into(),
            },
        }
    }

    #[test]
    fn earliest_sibling_year_picks_min_across_kinds() {
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:1@example\r
DTSTART:20260101T090000Z\r
DTEND:20260101T100000Z\r
SUMMARY:t\r
END:VEVENT\r
END:VCALENDAR\r
";
        let (_d1, event_path) = write_temp(ics, ".event.ics");
        let json = r#"{"checkinTime":"2027-01-01"}"#;
        let (_d2, res_path) = write_temp(json, ".reservation.json");

        let arts = vec![
            artifact(Kind::Event, event_path),
            artifact(Kind::Reservation, res_path),
        ];
        assert_eq!(earliest_sibling_year(&arts), Some(2026));
    }

    #[test]
    fn earliest_sibling_year_ignores_other_kinds() {
        let arts = vec![artifact(Kind::Ticket, PathBuf::from("ignored"))];
        assert_eq!(earliest_sibling_year(&arts), None);
    }

    /// LocalDir sinks must NOT consult or update seen.db; rewriting
    /// a tiny .ics file is cheaper than the lookup, and gating it
    /// would mean a deleted file (user reorganising on disk) stays
    /// missing forever.
    #[test]
    fn file_single_localdir_does_not_touch_seen_store() {
        let out_dir = tempfile::TempDir::new().unwrap();
        let sink = EventSinkKind::LocalDir(out_dir.path().to_path_buf());

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = SeenStore::open(&store_dir.path().join("seen.db")).unwrap();

        let event = SingleEvent {
            uid: "evt-1@example.com".to_string(),
            body: "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
                BEGIN:VEVENT\r\nUID:evt-1@example.com\r\n\
                DTSTAMP:20260101T120000Z\r\nDTSTART:20260201T100000Z\r\n\
                DTEND:20260201T110000Z\r\nSUMMARY:t\r\n\
                END:VEVENT\r\nEND:VCALENDAR\r\n"
                .to_string(),
            method: None,
        };

        let mut summary = Summary::default();
        file_single("ex", &event, &sink, Some(&store), &mut summary);

        assert_eq!(store.len().unwrap(), 0, "LocalDir must not mark seen.db");
        // local_events::file_single sanitises the UID into a
        // filename; exact form changes with the sanitiser, so just
        // assert that something landed under the dir.
        let entries: Vec<_> = std::fs::read_dir(out_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one event file, got {entries:?}"
        );
    }

    /// Point Sendmail at a binary that does not exist; the forward
    /// call must fail and the router must record a Failed stats event
    /// under the synthetic sink name so the dashboard can surface it.
    #[test]
    fn receipts_forward_failure_records_stats_event() {
        use crate::artifacts::{Artifact, Kind};
        use crate::stats::{self, Recorder};
        use crate::targets::mail_forward::MailForwarder;
        use crate::targets::receipts::ReceiptSink;

        let dir = tempfile::TempDir::new().unwrap();
        let log = dir.path().join("events.ndjson");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fwd = MailForwarder::sendmail(
            "mailsift@example.org",
            vec!["receipts@example.org".to_string()],
            Some("/nonexistent/mailsift-test-sendmail".to_string()),
            runtime.handle().clone(),
        )
        .unwrap();
        let sink = ReceiptSink::Forward(fwd);

        // A minimal receipt JSON on disk so file_receipt has something
        // to hand to the forwarder before it tries to spawn sendmail.
        let receipt_path = dir.path().join("dummy.receipt.json");
        std::fs::write(
            &receipt_path,
            r#"{"merchant":{"name":"m"},"orderNumber":"1"}"#,
        )
        .unwrap();
        let artifact = Artifact {
            path: receipt_path,
            kind: Kind::Receipt,
            slug: "dummy".to_string(),
            ext: "json".to_string(),
        };

        let mut summary = Summary::new(false);
        let recorder = Recorder::File(log.clone());

        file_receipt_artifact(
            "test-extractor",
            &artifact,
            b"From: sender@example.com\r\nSubject: t\r\n\r\nbody\r\n",
            &sink,
            None,
            &mut summary,
            &recorder,
            42,
        );

        let contents = std::fs::read_to_string(&log).expect("stats file should exist");
        let event: stats::Event =
            serde_json::from_str(contents.trim()).expect("one event was written");
        assert_eq!(event.extractor, RECEIPTS_FORWARD_SINK_NAME);
        assert_eq!(event.outcome, stats::Outcome::Failed);
        assert_eq!(event.ts, 42);
    }
}
