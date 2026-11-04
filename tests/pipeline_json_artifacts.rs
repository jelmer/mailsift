//! Pipeline-level test for the JSON records written for `reservation`
//! and `ticket` artifacts.
//!
//! A reservation is always converted to a calendar event; with
//! `--reservations-dir` the raw booking JSON is archived too. A ticket
//! is an opaque blob, so it gets a `.json` sidecar carrying what the
//! sibling reservation in the same run knows about it.
//!
//! Driven by a fixture extractor that emits both from one message, so
//! the sibling lookup is exercised rather than stubbed.

// The sidecar carries a `.meta.json` infix rather than a plain
// `.json`: a ticket blob's extension is arbitrary, so a
// `<slug>.ticket.json` attachment would otherwise land on the
// sidecar's own path.

use std::path::{Path, PathBuf};

mod common;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Run the fixture flight message through `replay`, returning the
/// reservations, tickets and events output dirs.
fn replay_flight() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/flight-confirmation.eml");
    let extractors = manifest.join("tests/fixtures/extractors");

    let out = tempfile::tempdir().expect("tempdir");
    let reservations = out.path().join("reservations");
    let tickets = out.path().join("tickets");
    let events = out.path().join("events");

    common::mailsift()
        .arg("replay")
        .arg(&eml)
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(&events)
        .arg("--reservations-dir")
        .arg(&reservations)
        .arg("--tickets-dir")
        .arg(&tickets)
        .assert()
        .success();

    (out, reservations, tickets, events)
}

#[test]
fn reservation_json_is_archived_under_year_and_provider() {
    let (_out, reservations, _tickets, _events) = replay_flight();

    let path = reservations.join("2026/fx-fx7qt2-fx123.json");
    let body =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

    assert_eq!(v["reservationNumber"], "FX7QT2");
    assert_eq!(v["underName"]["name"], "J Vernooij");
    assert_eq!(v["reservationFor"]["flightNumber"], "123");
    // receivedAt is stamped from the message Date: header.
    assert_eq!(v["receivedAt"], "2026-03-02T09:00:00Z");
}

#[test]
fn reservation_still_reaches_the_calendar() {
    let (_out, _reservations, _tickets, events) = replay_flight();

    // Archiving the JSON must not displace the calendar conversion.
    let entries: Vec<_> = std::fs::read_dir(&events)
        .expect("events dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1, "expected one event, got {entries:?}");
}

#[test]
fn ticket_sidecar_carries_sibling_reservation_fields() {
    let (_out, _reservations, tickets, _events) = replay_flight();

    // Year comes from the sibling reservation's departureTime, not
    // the message Date (March) - the trip is what matters.
    let blob = tickets.join("2026/fixture-air-fx123-2026-04-10.pdf");
    assert!(blob.exists(), "ticket blob missing at {}", blob.display());

    let path = tickets.join("2026/fixture-air-fx123-2026-04-10.meta.json");
    let body =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

    assert_eq!(v["slug"], "fixture-air-fx123-2026-04-10");
    assert_eq!(v["file"], "fixture-air-fx123-2026-04-10.pdf");
    assert_eq!(v["contentType"], "application/pdf");
    assert_eq!(v["reservationNumber"], "FX7QT2");
    assert_eq!(v["underName"], "J Vernooij");
    assert_eq!(v["provider"], "Fixture Air");
    assert_eq!(v["receivedAt"], "2026-03-02T09:00:00Z");
}

#[test]
fn reservations_dir_is_optional() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/flight-confirmation.eml");
    let extractors = manifest.join("tests/fixtures/extractors");
    let out = tempfile::tempdir().expect("tempdir");

    // Without --reservations-dir the run still succeeds and the
    // calendar conversion happens as before.
    common::mailsift()
        .arg("replay")
        .arg(&eml)
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .assert()
        .success();

    let entries: Vec<_> = std::fs::read_dir(out.path())
        .expect("events dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1, "expected one event, got {entries:?}");
}

/// Replay `eml` with the reservations, tickets and events dirs all
/// under `out`.
fn replay_into(eml: &Path, out: &Path) {
    common::mailsift()
        .arg("replay")
        .arg(eml)
        .arg("--extractors")
        .arg(manifest_dir().join("tests/fixtures/extractors"))
        .arg("--events-dir")
        .arg(out.join("events"))
        .arg("--reservations-dir")
        .arg(out.join("reservations"))
        .arg("--tickets-dir")
        .arg(out.join("tickets"))
        .assert()
        .success();
}

/// What each record filed for the fixture flight says about the
/// message it came from: the reservation's and the ticket sidecar's
/// `receivedAt`, and the event's `DTSTAMP`.
fn filed_dates(out: &Path) -> (String, String, String) {
    let json = |path: PathBuf| -> serde_json::Value {
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&body).expect("valid JSON")
    };
    let reservation = json(out.join("reservations/2026/fx-fx7qt2-fx123.json"));
    let sidecar = json(out.join("tickets/2026/fixture-air-fx123-2026-04-10.meta.json"));

    let events: Vec<_> = std::fs::read_dir(out.join("events"))
        .expect("events dir")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(events.len(), 1, "expected one event, got {events:?}");
    let event = std::fs::read_to_string(&events[0]).expect("read event");
    let dtstamp = event
        .lines()
        .find(|l| l.starts_with("DTSTAMP:"))
        .expect("event has a DTSTAMP");

    (
        reservation["receivedAt"].as_str().unwrap().to_string(),
        sidecar["receivedAt"].as_str().unwrap().to_string(),
        dtstamp.to_string(),
    )
}

fn dates(received_at: &str, dtstamp: &str) -> (String, String, String) {
    (
        received_at.to_string(),
        received_at.to_string(),
        format!("DTSTAMP:{dtstamp}"),
    )
}

/// The same booking arrives in three mails. Whichever order they are
/// processed in, what is filed comes from the one sent last.
#[test]
fn booking_mails_out_of_order_leave_the_newest() {
    let fixture = manifest_dir().join("tests/fixtures/eml/flight-confirmation.eml");
    let (_d1, earlier) =
        common::redated_fixture("flight-confirmation.eml", "Sun, 1 Mar 2026 09:00:00 +0000");
    let (_d2, later) =
        common::redated_fixture("flight-confirmation.eml", "Tue, 3 Mar 2026 09:00:00 +0000");
    let out = tempfile::tempdir().expect("tempdir");

    // Sent 2 March.
    replay_into(&fixture, out.path());
    assert_eq!(
        filed_dates(out.path()),
        dates("2026-03-02T09:00:00Z", "20260302T090000Z")
    );

    // An older one turns up afterwards: nothing moves.
    replay_into(&earlier, out.path());
    assert_eq!(
        filed_dates(out.path()),
        dates("2026-03-02T09:00:00Z", "20260302T090000Z")
    );

    // A newer one replaces all three.
    replay_into(&later, out.path());
    assert_eq!(
        filed_dates(out.path()),
        dates("2026-03-03T09:00:00Z", "20260303T090000Z")
    );

    // And a rescan of everything, oldest last, leaves it there.
    replay_into(&fixture, out.path());
    replay_into(&earlier, out.path());
    assert_eq!(
        filed_dates(out.path()),
        dates("2026-03-03T09:00:00Z", "20260303T090000Z")
    );
}

/// A refund mail names only the booking: it cancels the archived leg
/// and its event, and files nothing of its own.
#[test]
fn booking_cancellation_cancels_the_legs_filed_before_it() {
    let manifest = manifest_dir();
    let out = tempfile::tempdir().expect("tempdir");
    replay_into(
        &manifest.join("tests/fixtures/eml/flight-confirmation.eml"),
        out.path(),
    );
    replay_into(
        &manifest.join("tests/fixtures/eml/flight-refund.eml"),
        out.path(),
    );

    let names = |dir: PathBuf| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        names(out.path().join("reservations/2026")),
        vec!["fx-fx7qt2-fx123.json".to_string()]
    );
    assert_eq!(
        names(out.path().join("events")),
        vec!["flight-FX7QT2-FX123@mailsift.ics".to_string()]
    );
    let body = std::fs::read_to_string(out.path().join("reservations/2026/fx-fx7qt2-fx123.json"))
        .expect("read reservation");
    let record: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(record["reservationStatus"], "ReservationCancelled");
    assert_eq!(record["receivedAt"], "2026-03-04T09:00:00Z");
    let event = std::fs::read_to_string(out.path().join("events/flight-FX7QT2-FX123@mailsift.ics"))
        .expect("read event");
    assert_eq!(
        event.lines().find(|l| l.starts_with("STATUS:")),
        Some("STATUS:CANCELLED")
    );
}
