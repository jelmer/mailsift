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

use std::path::PathBuf;

use assert_cmd::Command;

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

    Command::cargo_bin("mailsift")
        .expect("binary built")
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

    let path = reservations.join("2026/fixture-air-fx7qt2.json");
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
    let blob = tickets.join("2026/boarding-pass.pdf");
    assert!(blob.exists(), "ticket blob missing at {}", blob.display());

    let path = tickets.join("2026/boarding-pass.json");
    let body =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

    assert_eq!(v["slug"], "boarding-pass");
    assert_eq!(v["file"], "boarding-pass.pdf");
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
    Command::cargo_bin("mailsift")
        .expect("binary built")
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
