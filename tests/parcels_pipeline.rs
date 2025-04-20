//! Pipeline-level tests for the `parcel` artifact path.
//!
//! These exercise *Rust-side* behaviour: the parcels target merging
//! successive status updates into one record (keyed by tracking
//! number) and a sibling `.reservation.json` being rendered into a
//! calendar event by the reservation converter.
//!
//! Both tests drive the binary with synthetic fixtures under
//! `tests/fixtures/` rather than any real vendor extractor — the
//! extractors directory is moving out of this repo and the Rust
//! pipeline must not depend on any specific extractor existing.

use std::path::PathBuf;

use assert_cmd::Command;

mod common;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_replay(eml: &str, events_dir: &PathBuf, parcels_dir: &PathBuf) {
    let manifest = manifest_dir();
    let extractors = manifest.join("tests/fixtures/extractors");
    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("replay")
        .arg(manifest.join("tests/fixtures/eml").join(eml))
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(events_dir)
        .arg("--parcels-dir")
        .arg(parcels_dir)
        .assert()
        .success();
}

/// Three parcel-status mails, processed in chronological order. The
/// parcels target merges them by tracking number into one on-disk
/// record whose `history` array captures every state seen, and whose
/// top-level fields reflect the latest update.
#[test]
fn parcels_target_merges_status_updates_into_history() {
    let events = tempfile::tempdir().expect("events tempdir");
    let parcels = tempfile::tempdir().expect("parcels tempdir");

    let events_dir = events.path().to_path_buf();
    let parcels_dir = parcels.path().to_path_buf();

    run_replay("parcel-on-its-way.eml", &events_dir, &parcels_dir);
    run_replay("parcel-out-for-delivery.eml", &events_dir, &parcels_dir);
    run_replay("parcel-delivered.eml", &events_dir, &parcels_dir);

    let parcel_path = parcels_dir.join("FIXT-12345.json");
    let body = std::fs::read_to_string(&parcel_path).expect("read parcel");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("parcel is valid JSON");

    assert_eq!(parsed["trackingNumber"], "FIXT-12345");
    // Latest status wins after the merge.
    assert_eq!(parsed["deliveryStatus"], "Delivered");
    assert_eq!(parsed["provider"]["name"], "Fixture Carrier");

    let history = parsed["history"].as_array().expect("history is an array");
    assert_eq!(history.len(), 3);
    assert_eq!(history[0]["deliveryStatus"], "OnItsWay");
    assert_eq!(history[1]["deliveryStatus"], "OutForDelivery");
    assert_eq!(history[2]["deliveryStatus"], "Delivered");
}

/// A `.reservation.json` emitted alongside a `.parcel.json` (when the
/// mail carries a delivery window) is converted by the reservation
/// renderer into an `EventReservation`-shaped iCalendar entry, with
/// UID keyed off the reservation id so a "your slot has moved" update
/// would replace it in place.
#[test]
fn parcel_delivery_window_renders_to_calendar_event() {
    let events = tempfile::tempdir().expect("events tempdir");
    let parcels = tempfile::tempdir().expect("parcels tempdir");

    let events_dir = events.path().to_path_buf();
    let parcels_dir = parcels.path().to_path_buf();

    run_replay("parcel-with-window.eml", &events_dir, &parcels_dir);

    let event_path = events_dir.join("event-WIN-9876@mailsift.ics");
    let event = common::read_event_stable(&event_path);
    let expected = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:ICALENDAR-RS\r
CALSCALE:GREGORIAN\r
BEGIN:VEVENT\r
DTEND:20260209T144000\r
DTSTART:20260209T134000\r
SUMMARY:Fixture delivery\r
UID:event-WIN-9876@mailsift\r
END:VEVENT\r
END:VCALENDAR\r
";
    assert_eq!(event, expected);
}
