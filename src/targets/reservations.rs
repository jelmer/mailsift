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
//! event start); the provider from the airline / hotel / venue name,
//! or for a flight from the airline's IATA code.
//! A follow-up mail about the same booking overwrites the record in
//! place, matching the UID-based replacement the calendar side does.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use tracing::warn;

use super::FileOutcome;
use super::json_target::{
    derive_year, first_non_empty, format_received_at, read_and_parse, received_at_in,
    received_at_of, write_unless_newer, year_from_iso_prefix,
};
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
    #[serde(default, rename = "@type")]
    kind: Option<Scalar>,
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
    /// schema.org's spelling; booking.com's JSON-LD uses this one.
    #[serde(default)]
    checkin_date: Option<Scalar>,
    #[serde(default)]
    start_time: Option<Scalar>,
    #[serde(default)]
    pickup_time: Option<Scalar>,
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

    /// Rendered value, but only when it isn't blank. Useful for `find`
    /// chains that want to skip empty scalars silently.
    fn non_empty(&self) -> Option<String> {
        self.as_str().filter(|s| !s.trim().is_empty())
    }
}

/// First scalar in `candidates` whose rendering is non-empty. The
/// preferred way to pull one string out of a set of alias fields.
fn first_non_empty_scalar<'a, I>(candidates: I) -> Option<String>
where
    I: IntoIterator<Item = Option<&'a Scalar>>,
{
    candidates.into_iter().flatten().find_map(Scalar::non_empty)
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

    fn iata_code(&self) -> Option<&str> {
        match self {
            Named::Object { iata_code, .. } => first_non_empty([iata_code.as_deref()]),
            Named::Text(_) | Named::Other(_) => None,
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
        first_non_empty_scalar([
            self.reservation_number.as_ref(),
            self.reservation_id.as_ref(),
            self.identifier.as_ref(),
        ])
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
        .or_else(|| for_.name.as_ref().and_then(Scalar::non_empty))
    }

    /// The provider part of the archive name. A flight goes by its
    /// airline's IATA code when it has one, so extractors spelling the
    /// airline's name differently still file a flight once.
    fn archive_provider(&self) -> Option<String> {
        let is_flight = self
            .kind
            .as_ref()
            .and_then(Scalar::as_str)
            .is_some_and(|k| k == "FlightReservation");
        is_flight
            .then(|| {
                self.reservation_for
                    .airline
                    .as_ref()
                    .and_then(Named::iata_code)
            })
            .flatten()
            .map(str::to_string)
            .or_else(|| self.provider())
    }

    /// The `YYYY-MM-DD` prefix of this leg's own date, when it has
    /// one. Distinguishes the outbound from the return of a return
    /// trip: both legs share the airline and the booking reference,
    /// so the date is the only thing that tells them apart.
    fn day(&self) -> Option<String> {
        let raw = self.date_candidates().next()?;
        let day = raw.trim().get(..10)?;
        let mut parts = day.split('-');
        let ok = matches!(parts.next(), Some(y) if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()))
            && parts.clone().count() == 2
            && parts.all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()));
        ok.then(|| day.to_string())
    }

    /// Date candidates, most specific first. Owned because a scalar
    /// may have been a JSON number that we rendered to a string.
    fn date_candidates(&self) -> impl Iterator<Item = String> + '_ {
        let for_ = &self.reservation_for;
        [
            for_.departure_time.as_ref(),
            self.checkin_time.as_ref(),
            self.checkin_date.as_ref(),
            self.start_time.as_ref(),
            self.pickup_time.as_ref(),
            for_.start_date.as_ref(),
            for_.door_time.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(Scalar::as_str)
    }

    /// The year to file this reservation under, from the first
    /// parseable date candidate; falls back to the current year.
    fn year(&self) -> i32 {
        let dates: Vec<String> = self.date_candidates().collect();
        if !dates.iter().any(|d| year_from_iso_prefix(d).is_some()) {
            warn!(
                number = self.number().as_deref().unwrap_or("-"),
                "reservation has no date we recognise; filing under the current year"
            );
        }
        derive_year(dates.iter().map(|s| Some(s.as_str())))
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
    let (_body, doc) = read_and_parse::<serde_json::Value>(src, "reservation")?;
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
        let leg_number = crate::reservation::leg_number(&leg);
        match file_one(
            src,
            dir,
            &reservation,
            &leg_body,
            received_at_epoch,
            leg_number.as_deref(),
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
    leg_number: Option<&str>,
    distinguish_by_day: bool,
) -> Result<FileOutcome> {
    let (year, number_slug, mut stem) =
        booking_stem(reservation).map_err(|e| anyhow!("{}: {e}", src.display()))?;
    // Legs of one itinerary share a booking reference and usually the
    // airline too, so the reference alone would file the return on top
    // of the outbound. A leg with a flight or train number is filed
    // under it, as its calendar event is, whichever mail it came from;
    // otherwise the leg's own date keeps them apart.
    if let Some(target) = leg_target(dir, year, &stem, leg_number) {
        let body_out = super::json_target::body_with_received_at(body, received_at_epoch);
        return write_unless_newer(&target, &body_out, "reservation");
    }
    if distinguish_by_day && let Some(day) = reservation.day() {
        stem = format!("{stem}-{day}");
    }
    // Some extractors emit one file per leg (KLM eticket, Air France,
    // British Airways, ...) with a per-leg suffix already baked into
    // the source filename, e.g. `klm-Y9ZO6Z-KL1012.reservation.json`
    // vs. `klm-Y9ZO6Z-KL1155.reservation.json`. Without a discriminator
    // both files would slug to the same target and clobber each other.
    // Honour whatever suffix the extractor appended past the booking
    // reference.
    if let Some(discriminator) = source_discriminator(src, &number_slug) {
        stem = format!("{stem}-{discriminator}");
    }

    let target = dir.join(format!("{year:04}")).join(format!("{stem}.json"));
    let body_out = super::json_target::body_with_received_at(body, received_at_epoch);
    write_unless_newer(&target, &body_out, "reservation")
}

/// The dash-separated tail of the source filename that follows the
/// booking reference, if any. `klm-Y9ZO6Z-KL1012.reservation.json` with
/// `number_slug = "y9zo6z"` yields `Some("kl1012")`. Returns `None` when
/// the source filename carries no extra content past the reference: the
/// caller keeps its provider-plus-reference stem in that case.
fn source_discriminator(src: &Path, number_slug: &str) -> Option<String> {
    let stem = src
        .file_name()?
        .to_str()?
        .strip_suffix(".reservation.json")?;
    let source_slug = slugify(stem, false);
    let idx = find_token(&source_slug, number_slug)?;
    let tail = source_slug
        .get(idx + number_slug.len()..)?
        .trim_matches('-');
    (!tail.is_empty()).then(|| tail.to_string())
}

/// Locate `needle` in `haystack` at a dash-or-start boundary and a
/// dash-or-end boundary. Prevents matching `abc` inside `xabc` or
/// `abcd`; the discriminator only makes sense when the booking
/// reference stands alone as its own token.
fn find_token(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        let idx = start + rel;
        let left_ok = idx == 0 || haystack.as_bytes()[idx - 1] == b'-';
        let end = idx + needle.len();
        let right_ok = end == haystack.len() || haystack.as_bytes()[end] == b'-';
        if left_ok && right_ok {
            return Some(idx);
        }
        start = idx + 1;
    }
    None
}

/// The year a reservation is filed under, its slugged booking
/// reference, and the `<provider>-<reference>` file stem.
fn booking_stem(reservation: &Reservation) -> Result<(i32, String, String)> {
    let number = reservation
        .number()
        .ok_or_else(|| anyhow!("missing 'reservationNumber'"))?;
    let number_slug = slugify(&number, false);
    if number_slug.is_empty() {
        bail!("empty slug after sanitisation (reservationNumber={number:?})");
    }
    // The provider is a nicety, not an identity: a booking reference
    // is already vendor-unique, so a reservation without a recognised
    // provider still files under the bare reference.
    let stem = match reservation.archive_provider().map(|p| slugify(&p, false)) {
        Some(provider_slug) if !provider_slug.is_empty() => {
            format!("{provider_slug}-{number_slug}")
        }
        _ => number_slug.clone(),
    };
    Ok((reservation.year(), number_slug, stem))
}

/// Where a leg with a flight or train number is filed.
fn leg_target(dir: &Path, year: i32, stem: &str, leg_number: Option<&str>) -> Option<PathBuf> {
    let leg = slugify(leg_number?, false);
    (!leg.is_empty()).then(|| {
        dir.join(format!("{year:04}"))
            .join(format!("{stem}-{leg}.json"))
    })
}

/// What [`reindex`] did, or would do, with one archived leg.
#[derive(Debug, PartialEq, Eq)]
pub enum Reindexed {
    Moved {
        from: PathBuf,
        to: PathBuf,
    },
    /// Another copy of the same leg was there already; the one from the
    /// newer mail is kept at `to`.
    Merged {
        from: PathBuf,
        to: PathBuf,
    },
}

/// Move archived legs filed before legs were named by their flight or
/// train number to where they are filed now, merging the copies one
/// leg collected under several names.
pub fn reindex(dir: &Path, dry_run: bool) -> Result<Vec<Reindexed>> {
    let mut done = Vec::new();
    // Targets a dry run would have filled, so it reports the merges a
    // real run would do.
    let mut claimed = HashSet::new();
    let mut paths = archived_records(dir)?;
    paths.sort();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let body = fs::read(&path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| anyhow!("parsing {}: {e}", path.display()))?;
        let Some(leg_number) = crate::reservation::leg_number(&value) else {
            continue;
        };
        let reservation: Reservation = serde_json::from_value(value)
            .map_err(|e| anyhow!("parsing {}: {e}", path.display()))?;
        let (year, _, stem) =
            booking_stem(&reservation).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        let Some(target) = leg_target(dir, year, &stem, Some(&leg_number)) else {
            continue;
        };
        if target == path {
            continue;
        }
        let merged = target.exists() || !claimed.insert(target.clone());
        if !dry_run {
            let incoming = received_at_in(&body)?;
            if !merged || !super::json_target::filed_from_newer(&target, incoming)? {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                write_atomic(&target, &body)?;
            }
            fs::remove_file(&path).map_err(|e| anyhow!("removing {}: {e}", path.display()))?;
        }
        let (from, to) = (path, target);
        done.push(if merged {
            Reindexed::Merged { from, to }
        } else {
            Reindexed::Moved { from, to }
        });
    }
    Ok(done)
}

/// Set on every leg of a reservation artifact that lists a booking's
/// whole itinerary, as a confirmation or a change confirmation does. A
/// mail about one leg (a check-in reminder) leaves it out.
pub const COMPLETE_ITINERARY_KEY: &str = "mailsift:itineraryComplete";

/// Mark the archived legs of a booking that a complete itinerary no
/// longer lists as cancelled.
///
/// A booking moved to another flight or train gets a new leg, because
/// a leg is filed under its flight or train number; without this the
/// leg it replaced would stay as if it were still booked. Only legs
/// filed from older mail are touched, and only for the bookings whose
/// legs in `src` all say they are the complete itinerary. Returns each
/// cancelled leg's path and its new contents, so the caller can update
/// its calendar event.
pub fn cancel_superseded_legs(
    src: &Path,
    dir: &Path,
    received_at_epoch: Option<i64>,
) -> Result<Vec<(PathBuf, serde_json::Value)>> {
    let (_body, doc) = read_and_parse::<serde_json::Value>(src, "reservation")?;
    let legs = split_legs(doc);
    let complete: HashSet<(String, String)> = legs
        .iter()
        .filter(|leg| leg.get(COMPLETE_ITINERARY_KEY) == Some(&serde_json::Value::Bool(true)))
        .filter_map(booking_key)
        .collect();
    if complete.is_empty() {
        return Ok(Vec::new());
    }
    let Some(received_at) = received_at_epoch.and_then(|e| chrono::DateTime::from_timestamp(e, 0))
    else {
        warn!(
            source = %src.display(),
            "complete itinerary without a message date; not cancelling the legs it drops"
        );
        return Ok(Vec::new());
    };
    let mut current = HashSet::new();
    for leg in &legs {
        for event in crate::reservation::convert(leg)? {
            current.insert(event.uid);
        }
    }

    let mut cancelled = Vec::new();
    for path in archived_records(dir)? {
        let body = fs::read(&path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        let mut record: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "skipping unreadable reservation");
                continue;
            }
        };
        if !booking_key(&record).is_some_and(|key| complete.contains(&key)) {
            continue;
        }
        let already_cancelled = record
            .get("reservationStatus")
            .and_then(serde_json::Value::as_str)
            .is_some_and(crate::reservation::is_cancelled_status);
        let filed_before = record
            .as_object()
            .and_then(received_at_of)
            .is_some_and(|filed| filed < received_at);
        if already_cancelled || !filed_before {
            continue;
        }
        let uids: Vec<String> = crate::reservation::convert(&record)?
            .into_iter()
            .map(|event| event.uid)
            .collect();
        if uids.is_empty() || uids.iter().any(|uid| current.contains(uid)) {
            continue;
        }
        let obj = record
            .as_object_mut()
            .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
        obj.insert("reservationStatus".into(), "ReservationCancelled".into());
        // Dated by the mail that dropped it, so replaying the original
        // confirmation doesn't put the leg back.
        if let Some(stamp) = format_received_at(received_at.timestamp()) {
            obj.insert("receivedAt".into(), stamp.into());
        }
        write_atomic(&path, serde_json::to_string_pretty(&record)?.as_bytes())?;
        cancelled.push((path, record));
    }
    Ok(cancelled)
}

/// The `@type` and booking reference a leg belongs to.
fn booking_key(leg: &serde_json::Value) -> Option<(String, String)> {
    let kind = leg.get("@type")?.as_str()?;
    let number = ["reservationNumber", "reservationId", "identifier"]
        .iter()
        .find_map(|k| leg.get(*k).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|n| !n.is_empty())?;
    Some((kind.to_string(), number.to_string()))
}

/// Every `<year>/<name>.json` under the reservations directory.
fn archived_records(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let years = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => bail!("reading {}: {e}", dir.display()),
    };
    for year in years {
        let year = year.map_err(|e| anyhow!("reading {}: {e}", dir.display()))?;
        if !year.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(year.path())? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                out.push(path);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Reservation {
        serde_json::from_value(json).expect("valid reservation")
    }

    fn year_of(r: &Reservation) -> i32 {
        r.year()
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

    /// schema.org's own spelling, used by booking.com's JSON-LD.
    #[test]
    fn year_from_lodging_checkin_date() {
        let r = parse(serde_json::json!({
            "reservationNumber": "H2",
            "checkinDate": "2015-08-01T15:00:00+09:00",
            "checkoutDate": "2015-08-02T10:00:00+09:00",
        }));
        assert_eq!(year_of(&r), 2015);
    }

    #[test]
    fn year_from_rental_car_pickup() {
        let r = parse(serde_json::json!({
            "@type": "RentalCarReservation",
            "reservationNumber": "C1",
            "pickupTime": "2015-03-02T09:00:00",
            "dropoffTime": "2015-03-05T09:00:00",
        }));
        assert_eq!(year_of(&r), 2015);
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

        let body = fs::read_to_string(dir.join("2026/u2-abc123.json")).unwrap();
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

    fn flight(number: &str, departs: &str, complete: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "@type": "FlightReservation",
            "reservationNumber": "AAA111",
            "reservationFor": {
                "@type": "Flight",
                "flightNumber": number,
                "airline": {"iataCode": "BA", "name": "British Airways"},
                "departureAirport": {"iataCode": "LHR"},
                "arrivalAirport": {"iataCode": "GVA"},
                "departureTime": departs
            }
        });
        if complete {
            v[COMPLETE_ITINERARY_KEY] = true.into();
        }
        v
    }

    /// Statuses of every archived leg, by flight number.
    fn statuses(dir: &Path) -> Vec<(String, Option<String>)> {
        let mut out: Vec<_> = archived_records(dir)
            .unwrap()
            .into_iter()
            .map(|path| {
                let v: serde_json::Value =
                    serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                (
                    v["reservationFor"]["flightNumber"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                    v["reservationStatus"].as_str().map(str::to_string),
                )
            })
            .collect();
        out.sort();
        out
    }

    /// File `legs` as one mail received at `epoch`, then run the
    /// superseded-leg pass for it; returns how many legs it cancelled.
    fn file_mail(tmp: &Path, dir: &Path, legs: &[serde_json::Value], epoch: i64) -> usize {
        let src = tmp.join(format!("mail-{epoch}.reservation.json"));
        fs::write(&src, serde_json::to_vec(legs).unwrap()).unwrap();
        file_reservation(&src, dir, Some(epoch)).unwrap();
        cancel_superseded_legs(&src, dir, Some(epoch))
            .unwrap()
            .len()
    }

    #[test]
    fn complete_itinerary_cancels_the_leg_it_moved_off() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        let booked = [
            flight("100", "2046-03-01T08:00:00Z", true),
            flight("200", "2046-03-08T18:00:00Z", true),
        ];
        assert_eq!(file_mail(tmp.path(), &dir, &booked, 1_000), 0);
        let changed = [
            flight("100", "2046-03-01T08:00:00Z", true),
            flight("300", "2046-03-08T20:00:00Z", true),
        ];
        assert_eq!(file_mail(tmp.path(), &dir, &changed, 2_000), 1);
        assert_eq!(
            statuses(&dir),
            vec![
                ("100".to_string(), None),
                ("200".to_string(), Some("ReservationCancelled".to_string())),
                ("300".to_string(), None),
            ]
        );
        // Replaying the original confirmation doesn't revive the leg.
        file_mail(tmp.path(), &dir, &booked, 1_000);
        assert_eq!(statuses(&dir)[1].1.as_deref(), Some("ReservationCancelled"));
    }

    #[test]
    fn a_leg_files_under_its_flight_whichever_mail_it_came_from() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        file_mail(
            tmp.path(),
            &dir,
            &[
                flight("100", "2046-03-01T08:00:00Z", true),
                flight("200", "2046-03-01T18:00:00Z", true),
            ],
            1_000,
        );
        file_mail(
            tmp.path(),
            &dir,
            &[flight("100", "2046-03-01T08:00:00Z", false)],
            2_000,
        );
        let mut names: Vec<String> = archived_records(&dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "ba-aaa111-ba100.json".to_string(),
                "ba-aaa111-ba200.json".to_string(),
            ]
        );
    }

    #[test]
    fn a_leg_files_once_however_its_flight_number_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        for (number, epoch) in [("0100", 1_000), ("BA100", 2_000), ("100", 3_000)] {
            file_mail(
                tmp.path(),
                &dir,
                &[flight(number, "2046-03-01T08:00:00Z", false)],
                epoch,
            );
        }
        assert_eq!(
            archived_records(&dir).unwrap(),
            vec![dir.join("2046/ba-aaa111-ba100.json")]
        );
    }

    #[test]
    fn a_flight_files_under_its_airline_code_however_the_airline_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        let mut renamed = flight("100", "2046-03-01T08:00:00Z", false);
        renamed["reservationFor"]["airline"]["name"] = "BRITISH AIRWAYS PLC".into();
        file_mail(
            tmp.path(),
            &dir,
            &[flight("100", "2046-03-01T08:00:00Z", false)],
            1_000,
        );
        file_mail(tmp.path(), &dir, &[renamed], 2_000);
        assert_eq!(
            archived_records(&dir).unwrap(),
            vec![dir.join("2046/ba-aaa111-ba100.json")]
        );
    }

    #[test]
    fn only_flights_file_under_an_airline_code() {
        let r = parse(serde_json::json!({
            "@type": "TrainReservation",
            "reservationNumber": "ABC123",
            "reservationFor": {"airline": {"iataCode": "BA", "name": "British Airways"}},
        }));
        assert_eq!(r.archive_provider().as_deref(), Some("British Airways"));
        let r = parse(serde_json::json!({
            "@type": "FlightReservation",
            "reservationNumber": "ABC123",
            "reservationFor": {"airline": {"name": "British Airways"}},
        }));
        assert_eq!(r.archive_provider().as_deref(), Some("British Airways"));
    }

    #[test]
    fn mail_about_one_leg_cancels_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        let booked = [
            flight("100", "2046-03-01T08:00:00Z", true),
            flight("200", "2046-03-08T18:00:00Z", true),
        ];
        file_mail(tmp.path(), &dir, &booked, 1_000);
        let reminder = [flight("100", "2046-03-01T08:00:00Z", false)];
        assert_eq!(file_mail(tmp.path(), &dir, &reminder, 2_000), 0);
        assert_eq!(
            statuses(&dir),
            vec![("100".to_string(), None), ("200".to_string(), None)]
        );
    }

    #[test]
    fn older_itinerary_leaves_newer_legs_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        let changed = [
            flight("100", "2046-03-01T08:00:00Z", true),
            flight("300", "2046-03-08T20:00:00Z", true),
        ];
        file_mail(tmp.path(), &dir, &changed, 2_000);
        // The original confirmation, processed later by a rescan.
        let booked = [
            flight("100", "2046-03-01T08:00:00Z", true),
            flight("200", "2046-03-08T18:00:00Z", true),
        ];
        assert_eq!(file_mail(tmp.path(), &dir, &booked, 1_000), 0);
        assert_eq!(
            statuses(&dir),
            vec![
                ("100".to_string(), None),
                ("200".to_string(), None),
                ("300".to_string(), None),
            ]
        );
    }

    #[test]
    fn reindex_moves_legs_to_their_flight_and_merges_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        fs::create_dir_all(dir.join("2046")).unwrap();
        let mut old = flight("100", "2046-03-01T08:00:00Z", false);
        old["receivedAt"] = "2046-01-01T00:00:00Z".into();
        let mut newer = old.clone();
        newer["receivedAt"] = "2046-02-01T00:00:00Z".into();
        fs::write(
            dir.join("2046/british-airways-aaa111.json"),
            serde_json::to_vec(&newer).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("2046/british-airways-aaa111-2046-03-01.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let target = dir.join("2046/ba-aaa111-ba100.json");

        let planned = reindex(&dir, true).unwrap();
        assert_eq!(
            planned,
            vec![
                Reindexed::Moved {
                    from: dir.join("2046/british-airways-aaa111-2046-03-01.json"),
                    to: target.clone(),
                },
                Reindexed::Merged {
                    from: dir.join("2046/british-airways-aaa111.json"),
                    to: target.clone(),
                },
            ]
        );
        assert_eq!(archived_records(&dir).unwrap().len(), 2);

        assert_eq!(reindex(&dir, false).unwrap(), planned);
        assert_eq!(archived_records(&dir).unwrap(), vec![target.clone()]);
        let kept: serde_json::Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        assert_eq!(kept["receivedAt"], "2046-02-01T00:00:00Z");
        assert_eq!(reindex(&dir, false).unwrap(), vec![]);
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

    /// A per-leg extractor (KLM eticket, Air France, ...) emits one
    /// file per flight with the flight number baked into the filename.
    /// The four files share a booking reference, so without honouring
    /// the source-filename suffix they'd all clobber one target.
    #[test]
    fn per_leg_source_filenames_keep_each_leg() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        for (fname, dep) in [
            ("klm-Y9ZO6Z-KL1012.reservation.json", "2026-09-21T17:10:00"),
            ("klm-Y9ZO6Z-KL1155.reservation.json", "2026-09-21T20:40:00"),
            ("klm-Y9ZO6Z-KL1154.reservation.json", "2026-09-28T17:10:00"),
            ("klm-Y9ZO6Z-KL1017.reservation.json", "2026-09-28T20:40:00"),
        ] {
            let src = tmp.path().join(fname);
            let body = format!(
                r#"{{"reservationNumber":"Y9ZO6Z","reservationFor":{{
                    "airline":{{"name":"KLM"}},"departureTime":"{dep}"}}}}"#
            );
            fs::write(&src, body).unwrap();
            file_reservation(&src, &dir, None).unwrap();
        }
        assert!(dir.join("2026/klm-y9zo6z-kl1012.json").exists());
        assert!(dir.join("2026/klm-y9zo6z-kl1155.json").exists());
        assert!(dir.join("2026/klm-y9zo6z-kl1154.json").exists());
        assert!(dir.join("2026/klm-y9zo6z-kl1017.json").exists());
    }

    /// The source filename carries no content past the reference (the
    /// typical single-leg case). The discriminator must stay off so
    /// follow-up mails update in place under the bare stem.
    #[test]
    fn source_stem_without_suffix_keeps_bare_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("klm-y9zo6z.reservation.json");
        fs::write(
            &src,
            br#"{"reservationNumber":"Y9ZO6Z","provider":"KLM",
                 "reservationFor":{"departureTime":"2026-09-21T17:10:00"}}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");

        file_reservation(&src, &dir, None).unwrap();
        assert!(dir.join("2026/klm-y9zo6z.json").exists());
    }

    /// The reference has to sit as its own token in the filename. A
    /// filename that just happens to contain the reference as a
    /// substring (or a longer alnum run that starts with it) must not
    /// trigger the discriminator.
    #[test]
    fn source_discriminator_needs_a_token_boundary() {
        assert_eq!(
            source_discriminator(Path::new("abc.reservation.json"), "abc"),
            None
        );
        assert_eq!(
            source_discriminator(Path::new("abcd.reservation.json"), "abc"),
            None
        );
        assert_eq!(
            source_discriminator(Path::new("xabc.reservation.json"), "abc"),
            None
        );
        assert_eq!(
            source_discriminator(Path::new("klm-abc-kl1.reservation.json"), "abc"),
            Some("kl1".to_string())
        );
        assert_eq!(
            source_discriminator(Path::new("abc-kl1.reservation.json"), "abc"),
            Some("kl1".to_string())
        );
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
    fn older_mail_does_not_replace_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("trip.reservation.json");
        let dir = tmp.path().join("out");

        // The amended booking, sent 2024-11-01, is processed first.
        fs::write(
            &src,
            br#"{"reservationNumber":"ABC123","checkinTime":"2024-11-06"}"#,
        )
        .unwrap();
        file_reservation(&src, &dir, Some(1730419200)).unwrap();
        // The original, sent a day earlier, turns up afterwards.
        fs::write(
            &src,
            br#"{"reservationNumber":"ABC123","checkinTime":"2024-11-05"}"#,
        )
        .unwrap();
        let outcomes = file_reservation(&src, &dir, Some(1730332800)).unwrap();
        assert!(matches!(outcomes[..], [FileOutcome::Kept(_)]));

        let body = fs::read_to_string(dir.join("2024/abc123.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["checkinTime"], "2024-11-06");
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
