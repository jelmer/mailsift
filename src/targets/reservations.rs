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
use tracing::info;

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
    reservation_number: Option<String>,
    #[serde(default)]
    reservation_id: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    under_name: Option<Named>,
    #[serde(default)]
    provider: Option<Named>,
    #[serde(default)]
    broker: Option<Named>,
    #[serde(default)]
    reservation_for: Option<ReservationFor>,
    #[serde(default)]
    checkin_time: Option<String>,
    #[serde(default)]
    start_time: Option<String>,
}

/// The nested `reservationFor`: a flight, trip, lodging or event.
/// Every variant contributes a possible provider name and a possible
/// start date; which fields are present depends on the type.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ReservationFor {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    airline: Option<Named>,
    #[serde(default)]
    departure_time: Option<String>,
    #[serde(default)]
    start_date: Option<String>,
    #[serde(default)]
    door_time: Option<String>,
}

/// A schema.org node that may be given as a bare string or as an
/// object with a `name` (and, for airlines, an `iataCode`).
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
}

impl Named {
    fn name(&self) -> Option<&str> {
        match self {
            Named::Text(s) => Some(s.as_str()),
            Named::Object { name, iata_code } => {
                first_non_empty([name.as_deref(), iata_code.as_deref()])
            }
        }
    }
}

impl Reservation {
    /// Booking reference. This is what makes a follow-up mail about
    /// the same trip overwrite the existing record.
    fn number(&self) -> Option<&str> {
        first_non_empty([
            self.reservation_number.as_deref(),
            self.reservation_id.as_deref(),
            self.identifier.as_deref(),
        ])
    }

    /// Who the booking is with. Prefers the airline (the `iataCode`
    /// makes for a tidy `ba-abc123`) over the generic provider, then
    /// the venue/hotel name, then the broker that sold it.
    fn provider(&self) -> Option<&str> {
        let for_ = self.reservation_for.as_ref();
        first_non_empty([
            for_.and_then(|f| f.airline.as_ref()).and_then(Named::name),
            self.provider.as_ref().and_then(Named::name),
            for_.and_then(|f| f.name.as_deref()),
            self.broker.as_ref().and_then(Named::name),
        ])
    }

    fn date_candidates(&self) -> [Option<&str>; 5] {
        let for_ = self.reservation_for.as_ref();
        [
            for_.and_then(|f| f.departure_time.as_deref()),
            self.checkin_time.as_deref(),
            self.start_time.as_deref(),
            for_.and_then(|f| f.start_date.as_deref()),
            for_.and_then(|f| f.door_time.as_deref()),
        ]
    }
}

/// One or many reservations. Extractors emitting JSON-LD sometimes
/// wrap even a single booking in a top-level array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Document {
    Many(Vec<Reservation>),
    One(Box<Reservation>),
}

impl Document {
    fn into_vec(self) -> Vec<Reservation> {
        match self {
            Document::One(r) => vec![*r],
            Document::Many(rs) => rs,
        }
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
    let parsed: Document = serde_json::from_str(&body).ok()?;
    let mut out = IdentifyingFields::default();
    for r in parsed.into_vec() {
        if out.reservation_number.is_none() {
            out.reservation_number = r.number().map(str::to_string);
        }
        if out.under_name.is_none() {
            out.under_name = r
                .under_name
                .as_ref()
                .and_then(Named::name)
                .map(str::to_string);
        }
        if out.provider.is_none() {
            out.provider = r.provider().map(str::to_string);
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
    let parsed: Document = serde_json::from_str(&body)
        .with_context(|| format!("parsing reservation JSON {}", src.display()))?;
    let reservations = parsed.into_vec();

    // Re-parse as generic JSON so each leg can be written back out
    // with its own fields intact rather than re-serialised from the
    // lossy struct above.
    let raw: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing reservation JSON {}", src.display()))?;
    let raw_legs: Vec<serde_json::Value> = match raw {
        serde_json::Value::Array(items) => items,
        other => vec![other],
    };

    let mut outcomes = Vec::with_capacity(reservations.len());
    for (index, reservation) in reservations.iter().enumerate() {
        let leg_body = raw_legs
            .get(index)
            .map(serde_json::to_string_pretty)
            .transpose()
            .context("re-serialising reservation leg")?
            .unwrap_or_else(|| body.clone());
        outcomes.push(file_one(
            src,
            dir,
            reservation,
            &leg_body,
            received_at_epoch,
        )?);
    }
    Ok(outcomes)
}

fn file_one(
    src: &Path,
    dir: &Path,
    reservation: &Reservation,
    body: &str,
    received_at_epoch: Option<i64>,
) -> Result<FileOutcome> {
    let number = reservation
        .number()
        .ok_or_else(|| anyhow!("{}: missing 'reservationNumber'", src.display()))?;
    let year = derive_year(reservation.date_candidates());

    let number_slug = slugify(number, false);
    if number_slug.is_empty() {
        bail!(
            "{}: empty slug after sanitisation (reservationNumber={number:?})",
            src.display()
        );
    }
    // The provider is a nicety, not an identity: a booking reference
    // is already vendor-unique, so a reservation without a recognised
    // provider still files under the bare reference.
    let stem = match reservation.provider().map(|p| slugify(p, false)) {
        Some(provider_slug) if !provider_slug.is_empty() => {
            format!("{provider_slug}-{number_slug}")
        }
        _ => number_slug,
    };

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

    #[test]
    fn provider_prefers_airline_iata_code() {
        let r = parse(serde_json::json!({
            "reservationNumber": "ABC123",
            "reservationFor": {"airline": {"iataCode": "BA", "name": "British Airways"}},
        }));
        assert_eq!(r.provider(), Some("British Airways"));
    }

    #[test]
    fn provider_falls_back_to_venue_name() {
        let r = parse(serde_json::json!({
            "reservationNumber": "9912345",
            "reservationFor": {"name": "Hilton Amsterdam"},
        }));
        assert_eq!(r.provider(), Some("Hilton Amsterdam"));
    }

    #[test]
    fn provider_accepts_bare_string_node() {
        let r = parse(serde_json::json!({
            "reservationNumber": "X1",
            "provider": "Nederlandse Spoorwegen",
        }));
        assert_eq!(r.provider(), Some("Nederlandse Spoorwegen"));
    }

    #[test]
    fn number_falls_back_through_aliases() {
        let r = parse(serde_json::json!({"reservationId": "RID-9"}));
        assert_eq!(r.number(), Some("RID-9"));
        let r = parse(serde_json::json!({"identifier": "ID-9"}));
        assert_eq!(r.number(), Some("ID-9"));
    }

    #[test]
    fn year_from_departure_time() {
        let r = parse(serde_json::json!({
            "reservationNumber": "ABC123",
            "reservationFor": {"departureTime": "2026-09-01T10:00:00Z"},
        }));
        assert_eq!(derive_year(r.date_candidates()), 2026);
    }

    #[test]
    fn year_from_lodging_checkin() {
        let r = parse(serde_json::json!({
            "reservationNumber": "H1",
            "checkinTime": "2026-08-15T15:00:00",
        }));
        assert_eq!(derive_year(r.date_candidates()), 2026);
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
        assert!(dir.join("2026/ba-aaa.json").exists());
        assert!(dir.join("2026/klm-aaa.json").exists());

        // Each leg keeps its own payload rather than the whole array.
        let body = fs::read_to_string(dir.join("2026/klm-aaa.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["reservationFor"]["airline"]["name"], "KLM");
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
