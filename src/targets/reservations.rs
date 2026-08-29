//! Local-directory target for `reservation` artifacts.
//!
//! Reservations are already converted to calendar events by
//! [`crate::reservation`], but the VEVENT keeps only what fits in an
//! iCalendar entry: summary, start/end, location. The booking
//! reference, the passenger name, the price and the rest of the
//! schema.org payload are dropped on that path. Filing the raw JSON
//! keeps the full record, the same way bills and receipts do.
//!
//! Each artifact is filed under
//! `<dir>/<year>/<provider-slug>-<reservation-number>.json`. The year
//! comes from the first date field we recognise (departure, check-in,
//! event start); the provider from the airline / hotel / venue name.
//! A follow-up mail about the same booking overwrites the record in
//! place, matching the UID-based replacement the calendar side does.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tracing::{info, warn};

use super::FileOutcome;
use super::json_target::{derive_year, first_non_empty};
use super::sink::{slugify, write_atomic};

/// Identifying fields we pull out of a `.reservation.json` artifact.
///
/// Deliberately looser than [`crate::reservation`]'s deserialiser: that
/// one has to understand dates well enough to build a VEVENT and
/// refuses input it can't render, whereas filing only needs a name, a
/// booking reference and a year prefix. Keeping them separate means an
/// exotic date format still gets its JSON archived even when the
/// calendar conversion gives up on it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reservation {
    #[serde(default)]
    reservation_number: Option<Scalar>,
    #[serde(default)]
    reservation_id: Option<Scalar>,
    #[serde(default)]
    identifier: Option<Scalar>,
    #[serde(default)]
    under_name: Option<Named>,
    #[serde(default)]
    provider: Option<Named>,
    #[serde(default)]
    broker: Option<Named>,
    /// Defaulted rather than optional: a `reservationFor` given in a
    /// shape we don't model (a bare string, say) deserialises to an
    /// empty struct instead of failing the document. Either way there
    /// is no nested provider or date to read.
    #[serde(default, deserialize_with = "lenient_reservation_for")]
    reservation_for: ReservationFor,
    #[serde(default)]
    checkin_time: Option<Scalar>,
    #[serde(default)]
    start_time: Option<Scalar>,
}

/// The nested `reservationFor`: a flight, trip, lodging or event.
/// Every variant contributes a possible provider name and a possible
/// start date; which fields are present depends on the type.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ReservationFor {
    #[serde(default)]
    name: Option<Scalar>,
    #[serde(default)]
    airline: Option<Named>,
    #[serde(default)]
    departure_time: Option<Scalar>,
    #[serde(default)]
    start_date: Option<Scalar>,
    #[serde(default)]
    door_time: Option<Scalar>,
}

/// A scalar that ought to be a string but is routinely a number:
/// booking references are often all digits, and some senders emit
/// dates as integers. Anything else (object, array, bool, null)
/// degrades to `None` rather than failing the whole document.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Scalar {
    Text(String),
    Number(serde_json::Number),
    /// Absorbs anything else; the payload is deliberately discarded.
    Other(serde::de::IgnoredAny),
}

impl Scalar {
    fn as_str(&self) -> Option<String> {
        match self {
            Scalar::Text(s) => Some(s.clone()),
            Scalar::Number(n) => Some(n.to_string()),
            Scalar::Other(_) => None,
        }
    }
}

/// A schema.org node that may be given as a bare string or as an
/// object with a `name` (and, for airlines, an `iataCode`).
///
/// The `Other` catch-all matters: this target's job is archiving, so a
/// sender putting an array in `underName` or a number in `iataCode`
/// must cost us that one field, not the whole record. Without it an
/// untagged enum fails the entire document and nothing gets filed.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Named {
    Text(String),
    Object {
        #[serde(default)]
        name: Option<String>,
        #[serde(default, rename = "iataCode")]
        iata_code: Option<String>,
    },
    /// Absorbs anything else; the payload is deliberately discarded.
    Other(serde::de::IgnoredAny),
}

impl Named {
    fn name(&self) -> Option<&str> {
        match self {
            Named::Text(s) => Some(s.as_str()),
            Named::Object { name, iata_code } => {
                first_non_empty([name.as_deref(), iata_code.as_deref()])
            }
            Named::Other(_) => None,
        }
    }
}

/// Deserialise `reservationFor`, falling back to an empty struct for
/// any shape we don't model. Keeps an odd nested value from sinking
/// the whole record.
fn lenient_reservation_for<'de, D>(de: D) -> std::result::Result<ReservationFor, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(de)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

impl Reservation {
    /// Booking reference. This is what makes a follow-up mail about
    /// the same trip overwrite the existing record.
    fn number(&self) -> Option<String> {
        [
            self.reservation_number.as_ref(),
            self.reservation_id.as_ref(),
            self.identifier.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(Scalar::as_str)
        .find(|s| !s.trim().is_empty())
    }

    /// Who the booking is with. Prefers the airline name, then the
    /// generic provider, then the venue/hotel, then the broker that
    /// sold it.
    fn provider(&self) -> Option<String> {
        let for_ = &self.reservation_for;
        first_non_empty([
            for_.airline.as_ref().and_then(Named::name),
            self.provider.as_ref().and_then(Named::name),
            self.broker.as_ref().and_then(Named::name),
        ])
        .map(str::to_string)
        .or_else(|| {
            for_.name
                .as_ref()
                .and_then(Scalar::as_str)
                .filter(|s| !s.trim().is_empty())
        })
    }

    /// The `YYYY-MM-DD` prefix of this leg's own date, when it has
    /// one. Distinguishes the outbound from the return of a return
    /// trip: both legs share the airline and the booking reference,
    /// so the date is the only thing that tells them apart.
    fn day(&self) -> Option<String> {
        let candidates = self.date_candidates();
        let raw = candidates.first()?.trim();
        let day = raw.get(..10)?;
        let mut parts = day.split('-');
        let ok = matches!(parts.next(), Some(y) if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()))
            && parts.clone().count() == 2
            && parts.all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()));
        ok.then(|| day.to_string())
    }

    /// Date candidates, most specific first. Owned because a scalar
    /// may have been a JSON number that we rendered to a string.
    fn date_candidates(&self) -> Vec<String> {
        let for_ = &self.reservation_for;
        [
            for_.departure_time.as_ref(),
            self.checkin_time.as_ref(),
            self.start_time.as_ref(),
            for_.start_date.as_ref(),
            for_.door_time.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(Scalar::as_str)
        .collect()
    }
}

/// Split a reservation document into its legs. Extractors emitting
/// JSON-LD sometimes wrap even a single booking in a top-level array,
/// so a bare object counts as a one-leg document.
fn split_legs(doc: serde_json::Value) -> Vec<serde_json::Value> {
    match doc {
        serde_json::Value::Array(items) => items,
        other => vec![other],
    }
}

/// Identifying fields lifted out of a reservation for the ticket
/// sidecar. Mirrors [`crate::targets::tickets::TicketMeta`], but owned
/// by this module since this is where the schema knowledge lives.
#[derive(Debug, Default)]
pub struct IdentifyingFields {
    pub reservation_number: Option<String>,
    pub under_name: Option<String>,
    pub provider: Option<String>,
}

/// Read a `.reservation.json` file and pull out the fields a ticket
/// sidecar wants. Returns `None` when the file can't be read or
/// parsed; the ticket still files, just without the extra context.
pub fn identifying_fields(path: &Path) -> Option<IdentifyingFields> {
    let body = fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&body).ok()?;
    let mut out = IdentifyingFields::default();
    for leg in split_legs(doc) {
        let Ok(r) = serde_json::from_value::<Reservation>(leg) else {
            continue;
        };
        if out.reservation_number.is_none() {
            out.reservation_number = r.number();
        }
        if out.under_name.is_none() {
            out.under_name = r
                .under_name
                .as_ref()
                .and_then(Named::name)
                .map(str::to_string);
        }
        if out.provider.is_none() {
            out.provider = r.provider();
        }
    }
    Some(out)
}

/// File a `.reservation.json` artifact.
///
/// A document holding several reservations (a multi-leg itinerary)
/// files one record per leg. They routinely share a booking reference,
/// so the leg's own provider and date keep the outbound and return
/// flights apart.
pub fn file_reservation(
    src: &Path,
    dir: &Path,
    received_at_epoch: Option<i64>,
) -> Result<Vec<FileOutcome>> {
    let body = fs::read_to_string(src)
        .with_context(|| format!("reading reservation source {}", src.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing reservation JSON {}", src.display()))?;
    // Each leg is kept as generic JSON so it can be written back out
    // with its own fields intact; the typed view below is only for
    // picking a filename, and is deliberately lossy.
    let raw_legs = split_legs(doc);

    // Legs are filed independently: one leg missing its booking
    // reference shouldn't discard the rest of the itinerary, and
    // aborting midway would leave a partial trip on disk with nothing
    // to say which half is missing. Per-leg failures are collected and
    // reported alongside whatever did file.
    let multi_leg = raw_legs.len() > 1;
    let mut outcomes = Vec::with_capacity(raw_legs.len());
    let mut failures = Vec::new();
    for (index, leg) in raw_legs.into_iter().enumerate() {
        let reservation: Reservation = match serde_json::from_value(leg.clone()) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("leg {index}: {e}"));
                continue;
            }
        };
        let leg_body = match serde_json::to_string_pretty(&leg) {
            Ok(b) => b,
            Err(e) => {
                failures.push(format!("leg {index}: re-serialising: {e}"));
                continue;
            }
        };
        match file_one(
            src,
            dir,
            &reservation,
            &leg_body,
            received_at_epoch,
            multi_leg,
        ) {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => failures.push(format!("leg {index}: {e:#}")),
        }
    }

    // Nothing filed at all: report it as the error it is rather than
    // returning an empty success the router would count as a no-op.
    if outcomes.is_empty() && !failures.is_empty() {
        bail!(
            "{}: no reservation filed ({})",
            src.display(),
            failures.join("; ")
        );
    }
    for failure in &failures {
        warn!(source = %src.display(), failure, "skipped reservation leg");
    }
    Ok(outcomes)
}

fn file_one(
    src: &Path,
    dir: &Path,
    reservation: &Reservation,
    body: &str,
    received_at_epoch: Option<i64>,
    distinguish_by_day: bool,
) -> Result<FileOutcome> {
    let number = reservation
        .number()
        .ok_or_else(|| anyhow!("{}: missing 'reservationNumber'", src.display()))?;
    let dates = reservation.date_candidates();
    let year = derive_year(dates.iter().map(|s| Some(s.as_str())));

    let number_slug = slugify(&number, false);
    if number_slug.is_empty() {
        bail!(
            "{}: empty slug after sanitisation (reservationNumber={number:?})",
            src.display()
        );
    }
    // The provider is a nicety, not an identity: a booking reference
    // is already vendor-unique, so a reservation without a recognised
    // provider still files under the bare reference.
    let mut stem = match reservation.provider().map(|p| slugify(&p, false)) {
        Some(provider_slug) if !provider_slug.is_empty() => {
            format!("{provider_slug}-{number_slug}")
        }
        _ => number_slug,
    };
    // Legs of one itinerary share a booking reference and usually the
    // airline too, so the reference alone would file the return on top
    // of the outbound. Append the leg's own date to keep them apart.
    if distinguish_by_day && let Some(day) = reservation.day() {
        stem = format!("{stem}-{day}");
    }

    let target = dir.join(format!("{year:04}")).join(format!("{stem}.json"));
    let existed = target.exists();
    let body_out = super::json_target::body_with_received_at(body, received_at_epoch);
    write_atomic(&target, body_out.as_bytes())?;

    let label = target.display().to_string();
    if existed {
        info!(target = %label, "reservation updated");
        Ok(FileOutcome::Updated(label))
    } else {
        info!(target = %label, "reservation created");
        Ok(FileOutcome::Created(label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Reservation {
        serde_json::from_value(json).expect("valid reservation")
    }

    fn year_of(r: &Reservation) -> i32 {
        let dates = r.date_candidates();
        derive_year(dates.iter().map(|s| Some(s.as_str())))
    }

    #[test]
    fn provider_prefers_airline_name_over_iata_code() {
        let r = parse(serde_json::json!({
            "reservationNumber": "ABC123",
            "reservationFor": {"airline": {"iataCode": "BA", "name": "British Airways"}},
        }));
        assert_eq!(r.provider().as_deref(), Some("British Airways"));
    }

    #[test]
    fn provider_uses_iata_code_when_airline_is_unnamed() {
        let r = parse(serde_json::json!({
            "reservationNumber": "ABC123",
            "reservationFor": {"airline": {"iataCode": "BA"}},
        }));
        assert_eq!(r.provider().as_deref(), Some("BA"));
    }

    #[test]
    fn provider_falls_back_to_venue_name() {
        let r = parse(serde_json::json!({
            "reservationNumber": "9912345",
            "reservationFor": {"name": "Hilton Amsterdam"},
        }));
        assert_eq!(r.provider().as_deref(), Some("Hilton Amsterdam"));
    }

    #[test]
    fn provider_accepts_bare_string_node() {
        let r = parse(serde_json::json!({
            "reservationNumber": "X1",
            "provider": "Nederlandse Spoorwegen",
        }));
        assert_eq!(r.provider().as_deref(), Some("Nederlandse Spoorwegen"));
    }

    #[test]
    fn number_falls_back_through_aliases() {
        let r = parse(serde_json::json!({"reservationId": "RID-9"}));
        assert_eq!(r.number().as_deref(), Some("RID-9"));
        let r = parse(serde_json::json!({"identifier": "ID-9"}));
        assert_eq!(r.number().as_deref(), Some("ID-9"));
    }

    #[test]
    fn year_from_departure_time() {
        let r = parse(serde_json::json!({
            "reservationNumber": "ABC123",
            "reservationFor": {"departureTime": "2026-09-01T10:00:00Z"},
        }));
        assert_eq!(year_of(&r), 2026);
    }

    #[test]
    fn year_from_lodging_checkin() {
        let r = parse(serde_json::json!({
            "reservationNumber": "H1",
            "checkinTime": "2026-08-15T15:00:00",
        }));
        assert_eq!(year_of(&r), 2026);
    }

    #[test]
    fn files_under_year_and_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"{
                "@type": "FlightReservation",
                "reservationNumber": "ABC123",
                "reservationFor": {
                    "airline": {"iataCode": "U2", "name": "easyJet"},
                    "departureTime": "2026-09-01T10:00:00Z"
                }
            }"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        let outcomes = file_reservation(&src, &dir, None).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], FileOutcome::Created(_)));

        let body = fs::read_to_string(dir.join("2026/easyjet-abc123.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["reservationNumber"], "ABC123");
    }

    #[test]
    fn refiling_same_booking_updates_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        let dir = tmp.path().join("out");
        fs::write(
            &src,
            br#"{"reservationNumber":"ABC123","checkinTime":"2026-08-15","provider":"Hilton"}"#,
        )
        .unwrap();
        let first = file_reservation(&src, &dir, None).unwrap();
        assert!(matches!(first[0], FileOutcome::Created(_)));

        let second = file_reservation(&src, &dir, None).unwrap();
        assert!(matches!(second[0], FileOutcome::Updated(_)));
    }

    #[test]
    fn multi_leg_itinerary_files_one_record_per_leg() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"[
                {"reservationNumber":"AAA","reservationFor":{
                    "airline":{"iataCode":"BA"},"departureTime":"2026-03-01T08:00:00Z"}},
                {"reservationNumber":"AAA","reservationFor":{
                    "airline":{"name":"KLM"},"departureTime":"2026-03-08T18:00:00Z"}}
            ]"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        let outcomes = file_reservation(&src, &dir, None).unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(dir.join("2026/ba-aaa-2026-03-01.json").exists());
        assert!(dir.join("2026/klm-aaa-2026-03-08.json").exists());

        // Each leg keeps its own payload rather than the whole array.
        let body = fs::read_to_string(dir.join("2026/klm-aaa-2026-03-08.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["reservationFor"]["airline"]["name"], "KLM");
    }

    /// The realistic return trip: same airline, same booking
    /// reference, same year. Only the date tells the legs apart, so
    /// without it the return would overwrite the outbound.
    #[test]
    fn return_trip_on_one_airline_keeps_both_legs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"[
                {"reservationNumber":"AAA","reservationFor":{
                    "airline":{"name":"KLM"},"departureTime":"2026-03-01T08:00:00Z"}},
                {"reservationNumber":"AAA","reservationFor":{
                    "airline":{"name":"KLM"},"departureTime":"2026-03-08T18:00:00Z"}}
            ]"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        let outcomes = file_reservation(&src, &dir, None).unwrap();
        assert_eq!(outcomes.len(), 2);

        let outbound = fs::read_to_string(dir.join("2026/klm-aaa-2026-03-01.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&outbound).unwrap();
        assert_eq!(v["reservationFor"]["departureTime"], "2026-03-01T08:00:00Z");

        let ret = fs::read_to_string(dir.join("2026/klm-aaa-2026-03-08.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&ret).unwrap();
        assert_eq!(v["reservationFor"]["departureTime"], "2026-03-08T18:00:00Z");
    }

    /// A single-leg document files under the bare reference: the date
    /// suffix only earns its place when there's another leg to
    /// disambiguate from, and a follow-up mail must still overwrite.
    #[test]
    fn single_leg_omits_the_date_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"{"reservationNumber":"AAA","provider":"KLM",
                 "reservationFor":{"departureTime":"2026-03-01T08:00:00Z"}}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        file_reservation(&src, &dir, None).unwrap();
        assert!(dir.join("2026/klm-aaa.json").exists());
    }

    #[test]
    fn one_bad_leg_does_not_discard_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"[
                {"reservationNumber":"AAA","checkinTime":"2026-03-01"},
                {"checkinTime":"2026-03-08"},
                {"reservationNumber":"CCC","checkinTime":"2026-03-15"}
            ]"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        let outcomes = file_reservation(&src, &dir, None).unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(dir.join("2026/aaa-2026-03-01.json").exists());
        assert!(dir.join("2026/ccc-2026-03-15.json").exists());
    }

    #[test]
    fn all_legs_failing_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(&src, br#"[{"checkinTime":"2026-03-01"}]"#).unwrap();
        let dir = tmp.path().join("out");

        let err = file_reservation(&src, &dir, None).unwrap_err();
        assert!(err.to_string().contains("no reservation filed"), "{err}");
    }

    #[test]
    fn numeric_booking_reference_is_accepted() {
        let r = parse(serde_json::json!({"reservationNumber": 12345}));
        assert_eq!(r.number().as_deref(), Some("12345"));
    }

    #[test]
    fn odd_field_shapes_do_not_sink_the_record() {
        // An array `underName`, a numeric `iataCode` and a bare-string
        // `reservationFor` each degrade to "field absent" rather than
        // failing the whole document.
        let r = parse(serde_json::json!({
            "reservationNumber": "X1",
            "underName": ["Jane Doe"],
            "reservationFor": "Fixture Inn",
            "checkinTime": "2026-04-01",
        }));
        assert_eq!(r.number().as_deref(), Some("X1"));
        assert_eq!(r.provider(), None);
        assert_eq!(year_of(&r), 2026);

        let r = parse(serde_json::json!({
            "reservationNumber": "X2",
            "reservationFor": {"airline": {"iataCode": 42}},
        }));
        assert_eq!(r.provider(), None);
    }

    #[test]
    fn day_requires_a_full_iso_date() {
        let r = parse(serde_json::json!({"reservationNumber":"X","checkinTime":"2026-04-01"}));
        assert_eq!(r.day().as_deref(), Some("2026-04-01"));

        let r = parse(serde_json::json!({"reservationNumber":"X","checkinTime":"2026-04"}));
        assert_eq!(r.day(), None);

        let r = parse(serde_json::json!({"reservationNumber":"X","checkinTime":"not-a-date"}));
        assert_eq!(r.day(), None);
    }

    #[test]
    fn files_without_provider_under_bare_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"{"reservationNumber":"XY9","startTime":"2026-05-05"}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        file_reservation(&src, &dir, None).unwrap();
        assert!(dir.join("2026/xy9.json").exists());
    }

    #[test]
    fn stamps_received_at() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"{"reservationNumber":"ABC123","checkinTime":"2024-11-05"}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        // 2024-11-01T00:00:00Z
        file_reservation(&src, &dir, Some(1730419200)).unwrap();
        let body = fs::read_to_string(dir.join("2024/abc123.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["receivedAt"], "2024-11-01T00:00:00Z");
    }

    #[test]
    fn identifying_fields_reads_booking_and_passenger() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(
            &src,
            br#"{
                "reservationNumber": "ABC123",
                "underName": {"name": "J Vernooij"},
                "reservationFor": {"airline": {"name": "easyJet"}}
            }"#,
        )
        .unwrap();

        let fields = identifying_fields(&src).unwrap();
        assert_eq!(fields.reservation_number.as_deref(), Some("ABC123"));
        assert_eq!(fields.under_name.as_deref(), Some("J Vernooij"));
        assert_eq!(fields.provider.as_deref(), Some("easyJet"));
    }

    #[test]
    fn identifying_fields_accepts_bare_string_under_name() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(&src, br#"{"reservationNumber":"X","underName":"Jane Doe"}"#).unwrap();

        let fields = identifying_fields(&src).unwrap();
        assert_eq!(fields.under_name.as_deref(), Some("Jane Doe"));
    }

    #[test]
    fn identifying_fields_returns_none_for_unparseable() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(&src, b"not json").unwrap();
        assert!(identifying_fields(&src).is_none());
    }

    #[test]
    fn missing_reservation_number_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        fs::write(&src, br#"{"checkinTime":"2026-08-15"}"#).unwrap();
        let dir = tmp.path().join("out");

        let err = file_reservation(&src, &dir, None).unwrap_err();
        assert!(
            err.to_string().contains("missing 'reservationNumber'"),
            "{err}"
        );
    }
}
