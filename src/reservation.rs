//! Convert schema.org reservation JSON into iCalendar `SingleEvent`s.
//!
//! Recognises the same set of reservation types the schema-ld extractor
//! emits: `FlightReservation`, `TrainReservation`, `BusReservation`,
//! `BoatReservation`, `LodgingReservation`, `EventReservation`,
//! `FoodEstablishmentReservation`. Unknown / unparseable inputs are
//! skipped silently so the caller can move on without aborting the batch.
//!
//! The UID is derived from the reservation identifier (typically
//! `reservationNumber`); this is what makes "follow-up mail with a
//! delay/change replaces the existing calendar event" work end to end.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use icalendar::{Calendar, Component, DatePerhapsTime, Event, EventLike};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::targets::SingleEvent;

/// One reservation as emitted by the schema-ld extractor. The
/// discriminator is the JSON-LD `@type` field; serde dispatches each
/// variant to its own struct, and an `Unknown` catch-all swallows
/// types we don't render so the caller doesn't have to special-case
/// them.
/// `#[serde(rename)]` keeps the JSON-LD `@type` values (`FlightReservation`,
/// ...) while letting the Rust variant names follow the
/// no-redundant-suffix style clippy prefers.
#[derive(Debug, Deserialize)]
#[serde(tag = "@type")]
enum Reservation {
    #[serde(rename = "FlightReservation")]
    Flight(FlightReservation),
    #[serde(rename = "TrainReservation")]
    Train(LineReservation),
    #[serde(rename = "BusReservation")]
    Bus(LineReservation),
    #[serde(rename = "BoatReservation")]
    Boat(LineReservation),
    #[serde(rename = "LodgingReservation")]
    Lodging(LodgingReservation),
    #[serde(rename = "EventReservation")]
    Event(EventReservation),
    #[serde(rename = "FoodEstablishmentReservation")]
    Food(FoodReservation),
    #[serde(other)]
    Unknown,
}

/// Identifier fields shared by every reservation. Pulled into its own
/// struct so each variant can `#[serde(flatten)]` them in.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ReservationId {
    #[serde(default)]
    reservation_number: Option<String>,
    #[serde(default)]
    reservation_id: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlightReservation {
    #[serde(flatten)]
    id: ReservationId,
    reservation_for: Flight,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Flight {
    #[serde(default)]
    flight_number: Option<String>,
    #[serde(default)]
    airline: Option<Airline>,
    #[serde(default)]
    departure_airport: Option<Place>,
    #[serde(default)]
    arrival_airport: Option<Place>,
    departure_time: DateTimeField,
    #[serde(default)]
    arrival_time: Option<DateTimeField>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Airline {
    #[serde(default)]
    iata_code: Option<String>,
    /// Exposed in fixtures (`Ryanair`) but not used in the summary
    /// line, which prefers the IATA code.
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

/// Train / bus / boat reservation. They share the same shape: a
/// "trip" with departure/arrival times and stations.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LineReservation {
    #[serde(flatten)]
    id: ReservationId,
    reservation_for: Trip,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Trip {
    #[serde(default)]
    train_number: Option<String>,
    #[serde(default)]
    bus_number: Option<String>,
    #[serde(default)]
    vehicle_name: Option<String>,
    /// Departure terminal. Schema.org spells it differently per
    /// transit mode; serde aliases give us a single field that
    /// accepts any of them.
    #[serde(
        default,
        alias = "departureStation",
        alias = "departureBusStop",
        alias = "departureBoatTerminal"
    )]
    departure_terminal: Option<Place>,
    #[serde(
        default,
        alias = "arrivalStation",
        alias = "arrivalBusStop",
        alias = "arrivalBoatTerminal"
    )]
    arrival_terminal: Option<Place>,
    departure_time: DateTimeField,
    #[serde(default)]
    arrival_time: Option<DateTimeField>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LodgingReservation {
    #[serde(flatten)]
    id: ReservationId,
    #[serde(default)]
    reservation_for: Option<Place>,
    /// Schema.org's `checkinTime` / `checkoutTime`. Many senders
    /// (Airbnb) use `checkinDate` / `checkoutDate` as date-only
    /// aliases; serde `alias` accepts either.
    #[serde(default, alias = "checkinDate")]
    checkin_time: Option<DateTimeField>,
    #[serde(default, alias = "checkoutDate")]
    checkout_time: Option<DateTimeField>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventReservation {
    #[serde(flatten)]
    id: ReservationId,
    reservation_for: EventDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventDetails {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    location: Option<EventLocation>,
    #[serde(default)]
    start_date: Option<DateTimeField>,
    #[serde(default)]
    door_time: Option<DateTimeField>,
    #[serde(default)]
    end_date: Option<DateTimeField>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventLocation {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Option<Address>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoodReservation {
    #[serde(flatten)]
    id: ReservationId,
    #[serde(default)]
    reservation_for: Option<Place>,
    start_time: DateTimeField,
    #[serde(default)]
    party_size: Option<PartySize>,
}

/// Generic schema.org `Place`-ish; used for airports, stations,
/// hotels, restaurants. We only ever read a few fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Place {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    iata_code: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    address: Option<Address>,
}

/// `partySize` is either a plain integer or a textual count (`"two"`,
/// `"4 people"`). Accept both.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PartySize {
    Number(u64),
    Text(String),
}

impl fmt::Display for PartySize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PartySize::Number(n) => write!(f, "{n}"),
            PartySize::Text(s) => write!(f, "{s}"),
        }
    }
}

/// `address` can be a bare string or a `PostalAddress` object.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Address {
    Text(String),
    Postal(PostalAddress),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostalAddress {
    #[serde(default)]
    street_address: Option<String>,
    #[serde(default)]
    address_locality: Option<String>,
    #[serde(default)]
    address_country: Option<String>,
}

impl Address {
    fn render(&self) -> Option<String> {
        match self {
            Address::Text(s) => Some(s.clone()),
            Address::Postal(p) => {
                let parts: Vec<&str> = [
                    p.street_address.as_deref(),
                    p.address_locality.as_deref(),
                    p.address_country.as_deref(),
                ]
                .into_iter()
                .flatten()
                .collect();
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join(", "))
                }
            }
        }
    }
}

/// A date-time field that may arrive in one of several flavours:
///
/// - RFC 3339 with explicit offset (`2024-07-12T18:45:00+00:00`,
///   trailing `Z` is also OK)
/// - Naive ISO 8601 (`2024-07-12T18:45:00`), treated as floating
///   local time
/// - Tebi's `InstantLocalizable(instant YY-MM-DDTHH:MM:SS[.fff]Z,
///   timeZone=..., style=...)` wrapper; the year is two digits and
///   we treat it as `20YY` (Tebi appeared after the millennium and
///   there's no graceful way to disambiguate)
#[derive(Debug)]
enum DateTimeField {
    Zoned(DateTime<FixedOffset>),
    Floating(NaiveDateTime),
}

impl<'de> Deserialize<'de> for DateTimeField {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        parse_date_time(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("unrecognised reservation date-time: {s:?}"))
        })
    }
}

fn parse_date_time(raw: &str) -> Option<DateTimeField> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(DateTimeField::Zoned(dt));
    }
    if let Some(trimmed) = s.strip_suffix('Z')
        && let Ok(dt) = DateTime::parse_from_rfc3339(&format!("{trimmed}+00:00"))
    {
        return Some(DateTimeField::Zoned(dt));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(DateTimeField::Floating(dt));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Some(DateTimeField::Floating(dt));
    }
    parse_instant_localizable(s)
}

/// Recognise Tebi's `InstantLocalizable(instant YY-MM-DDTHH:MM:SS[.fff]Z,
/// timeZone=..., style=...)` and pull the inner instant out as a zoned
/// datetime in UTC.
fn parse_instant_localizable(s: &str) -> Option<DateTimeField> {
    let inner = s
        .strip_prefix("InstantLocalizable(instant ")?
        .split_once(',')
        .map(|(head, _)| head)?
        .trim();
    let zless = inner.strip_suffix('Z')?;
    let (date_part, time_part) = zless.split_once('T')?;
    let (year_str, rest) = date_part.split_once('-')?;
    if year_str.len() != 2 || !year_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let canonical = format!("20{year_str}-{rest}T{time_part}+00:00");
    DateTime::parse_from_rfc3339(&canonical)
        .ok()
        .map(DateTimeField::Zoned)
}

impl From<&DateTimeField> for DatePerhapsTime {
    fn from(p: &DateTimeField) -> Self {
        match p {
            DateTimeField::Zoned(dt) => dt.with_timezone(&Utc).into(),
            DateTimeField::Floating(dt) => (*dt).into(),
        }
    }
}

/// Deserialise a reservation document that may be either a single
/// JSON object or a top-level array, into a flat `Vec<Reservation>`.
///
/// Implemented by hand (rather than as an untagged enum) to avoid the
/// `large_enum_variant` lint that would fire on `One(Reservation) |
/// Many(Vec<Reservation>)`; `Reservation` is several hundred bytes
/// while `Vec` is 24, and boxing would just trade one indirection for
/// another with no real benefit.
fn deserialise_one_or_many(json: serde_json::Value) -> Result<Vec<Reservation>> {
    if json.is_array() {
        let rs: Vec<Reservation> = serde_json::from_value(json)
            .context("schema.org reservation array didn't match the expected shape")?;
        Ok(rs)
    } else {
        let r: Reservation = serde_json::from_value(json)
            .context("schema.org reservation JSON didn't match the expected shape")?;
        Ok(vec![r])
    }
}

/// Convert a parsed reservation document into zero or more VEVENT
/// calendar bodies. Each `Reservation` variant maps to at most one
/// event; unknown types and variants whose required fields are
/// missing are skipped.
pub fn convert(json: &serde_json::Value) -> Result<Vec<SingleEvent>> {
    let parsed = deserialise_one_or_many(json.clone())?;
    Ok(parsed.into_iter().filter_map(render).collect())
}

/// Parse a `.reservation.json` file from disk and convert.
pub fn convert_file(path: &Path) -> Result<Vec<SingleEvent>> {
    let body =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing reservation JSON {}", path.display()))?;
    let parsed = deserialise_one_or_many(json)?;
    Ok(parsed.into_iter().filter_map(render).collect())
}

/// Render one reservation to a `SingleEvent`. Returns `None` for
/// `Unknown` types or for cases where the required date/time isn't
/// parseable.
fn render(r: Reservation) -> Option<SingleEvent> {
    let (prefix, summary, dtstart, dtend, location, id) = match r {
        Reservation::Flight(f) => {
            let summary = flight_summary(&f.reservation_for);
            let dep_label = airport_label(f.reservation_for.departure_airport.as_ref());
            (
                "flight",
                summary,
                f.reservation_for.departure_time,
                f.reservation_for.arrival_time,
                dep_label,
                f.id,
            )
        }
        Reservation::Train(t) => line_event("Train", t),
        Reservation::Bus(b) => line_event("Bus", b),
        Reservation::Boat(b) => line_event("Sailing", b),
        Reservation::Lodging(l) => {
            let name = l
                .reservation_for
                .as_ref()
                .and_then(|p| p.name.clone())
                .unwrap_or_else(|| "Hotel".to_string());
            let location = l
                .reservation_for
                .as_ref()
                .and_then(|p| p.address.as_ref())
                .and_then(Address::render);
            let dtstart = l.checkin_time?;
            (
                "hotel",
                format!("Stay at {name}"),
                dtstart,
                l.checkout_time,
                location,
                l.id,
            )
        }
        Reservation::Event(e) => {
            let summary = e
                .reservation_for
                .name
                .clone()
                .unwrap_or_else(|| "Event".to_string());
            let dtstart = e
                .reservation_for
                .start_date
                .or(e.reservation_for.door_time)?;
            let location = event_location_string(e.reservation_for.location.as_ref());
            (
                "event",
                summary,
                dtstart,
                e.reservation_for.end_date,
                location,
                e.id,
            )
        }
        Reservation::Food(f) => {
            let name = f
                .reservation_for
                .as_ref()
                .and_then(|p| p.name.clone())
                .unwrap_or_else(|| "Restaurant".to_string());
            let summary = match f.party_size.as_ref() {
                Some(p) => format!("{name} ({p})"),
                None => name,
            };
            let location = f
                .reservation_for
                .as_ref()
                .and_then(|p| p.address.as_ref())
                .and_then(Address::render);
            ("restaurant", summary, f.start_time, None, location, f.id)
        }
        Reservation::Unknown => return None,
    };

    let uid = uid_for(prefix, &id, &summary, &dtstart);
    let body = render_ics(
        &uid,
        &dtstart,
        dtend.as_ref(),
        &summary,
        location.as_deref(),
    );
    Some(SingleEvent {
        uid,
        body,
        method: None,
    })
}

fn flight_summary(f: &Flight) -> String {
    let flight_no = f.flight_number.as_deref().unwrap_or_default();
    let airline_code = f
        .airline
        .as_ref()
        .and_then(|a| a.iata_code.as_deref())
        .unwrap_or("");
    let code = format!("{airline_code}{flight_no}");
    let dep = airport_label(f.departure_airport.as_ref());
    let arr = airport_label(f.arrival_airport.as_ref());
    if code.is_empty() {
        format!(
            "Flight {} -> {}",
            dep.as_deref().unwrap_or("?"),
            arr.as_deref().unwrap_or("?")
        )
    } else {
        format!(
            "Flight {code}: {} -> {}",
            dep.as_deref().unwrap_or("?"),
            arr.as_deref().unwrap_or("?")
        )
    }
}

fn line_event(
    kind: &str,
    r: LineReservation,
) -> (
    &'static str,
    String,
    DateTimeField,
    Option<DateTimeField>,
    Option<String>,
    ReservationId,
) {
    let prefix: &'static str = match kind {
        "Train" => "train",
        "Bus" => "bus",
        "Sailing" => "boat",
        _ => "trip",
    };
    let number = r
        .reservation_for
        .train_number
        .as_deref()
        .or(r.reservation_for.bus_number.as_deref())
        .or(r.reservation_for.vehicle_name.as_deref())
        .unwrap_or("");
    let dep_label = station_label(r.reservation_for.departure_terminal.as_ref());
    let arr_label = station_label(r.reservation_for.arrival_terminal.as_ref());
    let lead = if number.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} {number}")
    };
    let summary = format!(
        "{lead}: {} -> {}",
        dep_label.as_deref().unwrap_or("?"),
        arr_label.as_deref().unwrap_or("?")
    );
    (
        prefix,
        summary,
        r.reservation_for.departure_time,
        r.reservation_for.arrival_time,
        dep_label,
        r.id,
    )
}

fn airport_label(place: Option<&Place>) -> Option<String> {
    let place = place?;
    let code = place
        .iata_code
        .as_deref()
        .or(place.identifier.as_deref())
        .map(str::to_string);
    let name = place.name.clone();
    match (code, name) {
        (Some(c), Some(n)) => Some(format!("{c} ({n})")),
        (Some(c), None) => Some(c),
        (None, Some(n)) => Some(n),
        (None, None) => None,
    }
}

fn station_label(place: Option<&Place>) -> Option<String> {
    let place = place?;
    place.name.clone().or_else(|| place.identifier.clone())
}

fn event_location_string(loc: Option<&EventLocation>) -> Option<String> {
    let loc = loc?;
    let venue = loc.name.clone();
    let addr = loc.address.as_ref().and_then(Address::render);
    match (venue, addr) {
        (Some(v), Some(a)) => Some(format!("{v}, {a}")),
        (Some(v), None) => Some(v),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

fn uid_for(prefix: &str, id: &ReservationId, summary: &str, dtstart: &DateTimeField) -> String {
    if let Some(value) = id
        .reservation_number
        .as_deref()
        .or(id.reservation_id.as_deref())
        .or(id.identifier.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("{prefix}-{value}@mailsift");
    }
    // Fallback: stable hash of the rendered summary and dtstart. The
    // typed model has already discarded fields we don't read, so we
    // can't hash the source JSON faithfully; summary + start is
    // distinctive enough for dedup within a feed.
    let mut hasher = Sha256::new();
    hasher.update(summary.as_bytes());
    match dtstart {
        DateTimeField::Zoned(dt) => hasher.update(dt.to_rfc3339().as_bytes()),
        DateTimeField::Floating(dt) => hasher.update(dt.to_string().as_bytes()),
    }
    let hex = format!("{:x}", hasher.finalize());
    format!("{prefix}-{}@mailsift", &hex[..16])
}

fn render_ics(
    uid: &str,
    dtstart: &DateTimeField,
    dtend: Option<&DateTimeField>,
    summary: &str,
    location: Option<&str>,
) -> String {
    let mut event = Event::new();
    let start: DatePerhapsTime = dtstart.into();
    event
        .uid(uid)
        .timestamp(Utc::now())
        .starts(start)
        .summary(summary);
    if let Some(end) = dtend {
        let end: DatePerhapsTime = end.into();
        event.ends(end);
    }
    if let Some(loc) = location {
        event.location(loc);
    }
    let mut calendar = Calendar::new();
    calendar.push(event.done());
    calendar.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn first(events: Vec<SingleEvent>) -> SingleEvent {
        assert_eq!(events.len(), 1, "expected exactly one event");
        events.into_iter().next().unwrap()
    }

    #[test]
    fn flight_basic() {
        let v = json!({
            "@type": "FlightReservation",
            "reservationNumber": "ABCDEFG",
            "reservationFor": {
                "@type": "Flight",
                "flightNumber": "1234",
                "airline": {"iataCode": "FR", "name": "Ryanair"},
                "departureAirport": {"iataCode": "DUB", "name": "Dublin"},
                "arrivalAirport": {"iataCode": "BCN", "name": "Barcelona"},
                "departureTime": "2026-07-20T18:00:00+00:00",
                "arrivalTime": "2026-07-20T22:00:00+02:00"
            }
        });
        let ev = first(convert(&v).unwrap());
        assert_eq!(ev.uid, "flight-ABCDEFG@mailsift");
        assert!(
            ev.body
                .contains("SUMMARY:Flight FR1234: DUB (Dublin) -> BCN (Barcelona)")
        );
        assert!(ev.body.contains("DTSTART:20260720T180000Z"));
        assert!(ev.body.contains("DTEND:20260720T200000Z"));
    }

    #[test]
    fn lodging_floating_times() {
        let v = json!({
            "@type": "LodgingReservation",
            "reservationNumber": "9999999999",
            "checkinTime": "2024-12-27T15:00:00",
            "checkoutTime": "2024-12-30T11:00:00",
            "reservationFor": {
                "name": "Ruby Lotti Hotel Hamburg",
                "address": "1-3 Düsternstraße, Hamburg Neustadt, 20355 Hamburg, Germany"
            }
        });
        let ev = first(convert(&v).unwrap());
        assert_eq!(ev.uid, "hotel-9999999999@mailsift");
        assert!(ev.body.contains("DTSTART:20241227T150000"));
        assert!(ev.body.contains("DTEND:20241230T110000"));
        assert!(ev.body.contains("LOCATION:1-3 Düsternstraße"));
        assert!(ev.body.contains("SUMMARY:Stay at Ruby Lotti Hotel Hamburg"));
    }

    #[test]
    fn unknown_type_returns_empty() {
        let v = json!({"@type": "EmailMessage"});
        assert!(convert(&v).unwrap().is_empty());
    }

    #[test]
    fn tebi_instant_localizable_food_reservation() {
        // Tebi serialises `startTime` as the debug repr of an
        // `InstantLocalizable` rather than as an ISO 8601 string. Our
        // parser should peel the inner instant out and treat the
        // (two-digit) year as 20YY.
        let v = json!({
            "@type": "FoodEstablishmentReservation",
            "reservationNumber": "063e9d6f-0371-4e65-89f0-10c69f9740bf",
            "startTime": "InstantLocalizable(instant 26-03-12T18:00:00Z, timeZone=Europe/Amsterdam, style=ShortDateTime)",
            "partySize": 2,
            "reservationFor": {
                "@type": "FoodEstablishment",
                "name": "BROEI"
            }
        });
        let ev = first(convert(&v).unwrap());
        assert_eq!(
            ev.uid,
            "restaurant-063e9d6f-0371-4e65-89f0-10c69f9740bf@mailsift"
        );
        assert_eq!(
            strip_dtstamp(&ev.body),
            "BEGIN:VCALENDAR\r\n\
             VERSION:2.0\r\n\
             PRODID:ICALENDAR-RS\r\n\
             CALSCALE:GREGORIAN\r\n\
             BEGIN:VEVENT\r\n\
             DTSTART:20260312T180000Z\r\n\
             SUMMARY:BROEI (2)\r\n\
             UID:restaurant-063e9d6f-0371-4e65-89f0-10c69f9740bf@mailsift\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR\r\n",
        );
    }

    #[test]
    fn top_level_array_produces_multiple_events() {
        let v = json!([
            {
                "@type": "FlightReservation",
                "reservationNumber": "AAA",
                "reservationFor": {
                    "flightNumber": "100",
                    "airline": {"iataCode": "BA"},
                    "departureAirport": {"iataCode": "LHR"},
                    "arrivalAirport": {"iataCode": "JFK"},
                    "departureTime": "2026-01-01T10:00:00Z"
                }
            },
            {
                "@type": "FlightReservation",
                "reservationNumber": "BBB",
                "reservationFor": {
                    "flightNumber": "200",
                    "airline": {"iataCode": "BA"},
                    "departureAirport": {"iataCode": "JFK"},
                    "arrivalAirport": {"iataCode": "LHR"},
                    "departureTime": "2026-01-08T22:00:00Z"
                }
            }
        ]);
        let events = convert(&v).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].uid, "flight-AAA@mailsift");
        assert_eq!(events[1].uid, "flight-BBB@mailsift");
    }

    #[test]
    fn party_size_as_text() {
        let v = json!({
            "@type": "FoodEstablishmentReservation",
            "reservationNumber": "x",
            "startTime": "2026-01-01T19:00:00Z",
            "partySize": "four",
            "reservationFor": {"name": "Diner"}
        });
        let ev = first(convert(&v).unwrap());
        assert!(ev.body.contains("SUMMARY:Diner (four)"));
    }

    /// Drop the wall-clock `DTSTAMP:` line so the rendered ICS body
    /// can be compared with `assert_eq!`.
    fn strip_dtstamp(body: &str) -> String {
        body.lines()
            .filter(|l| !l.starts_with("DTSTAMP:"))
            .collect::<Vec<_>>()
            .join("\r\n")
            + "\r\n"
    }
}
