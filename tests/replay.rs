use std::path::PathBuf;

use assert_cmd::Command;

mod common;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn replay_files_ics_passthrough_event() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/ics-attachment.eml");
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

    let path = out.path().join("fixture-ics-1@example.ics");
    let actual = common::read_event_stable(&path);
    // split_calendar re-serializes with its own PRODID and orders
    // fields alphabetically, which is why DTEND precedes DTSTART here.
    let expected = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:ICALENDAR-RS\r
CALSCALE:GREGORIAN\r
BEGIN:VEVENT\r
DTEND:20260720T200000Z\r
DTSTART:20260720T180000Z\r
SUMMARY:Fixture reservation\r
UID:fixture-ics-1@example.com\r
END:VEVENT\r
END:VCALENDAR\r
";
    assert_eq!(actual, expected);
}

#[test]
fn replay_fails_with_empty_extractors_dir() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/ics-attachment.eml");
    let empty = tempfile::tempdir().expect("tempdir");
    let events = tempfile::tempdir().expect("tempdir");

    let output = Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("replay")
        .arg(&eml)
        .arg("--extractors")
        .arg(empty.path())
        .arg("--events-dir")
        .arg(events.path())
        .assert()
        .failure()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("no extractors found"),
        "unexpected stderr: {stderr}"
    );
}
