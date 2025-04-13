//! Pipeline-level test: schema.org reservation JSON -> iCalendar
//! conversion.
//!
//! Driven by a synthetic fixture extractor that emits a
//! `LodgingReservation` JSON for a hand-crafted EML, so the test
//! exercises the Rust converter end to end without depending on any
//! specific real extractor.

use std::path::PathBuf;

use assert_cmd::Command;

mod common;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn lodging_reservation_renders_to_ics() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/lodging-confirmation.eml");
    let extractors = manifest.join("tests/fixtures/extractors");

    let out = tempfile::tempdir().expect("tempdir");

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

    let expected_path = out.path().join("hotel-LDG-7777@mailsift.ics");
    let actual = common::read_event_stable(&expected_path);
    let expected = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:ICALENDAR-RS\r
CALSCALE:GREGORIAN\r
BEGIN:VEVENT\r
DTEND:20260412T110000\r
DTSTART:20260410T150000\r
LOCATION:1 Example Street\\, Amsterdam\\, NL\r
SUMMARY:Stay at Fixture Inn\r
UID:hotel-LDG-7777@mailsift\r
END:VEVENT\r
END:VCALENDAR\r
";
    assert_eq!(actual, expected);
}
