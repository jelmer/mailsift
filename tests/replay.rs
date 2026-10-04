use std::path::PathBuf;

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

    common::mailsift()
        .arg("replay")
        .arg(&eml)
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .assert()
        .success();

    let path = out.path().join("fixture-ics-1@example.ics");
    let actual = std::fs::read_to_string(&path).expect("read event");
    // split_calendar re-serializes with its own PRODID and orders
    // fields alphabetically, which is why DTEND precedes DTSTART here.
    let expected = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:ICALENDAR-RS\r
CALSCALE:GREGORIAN\r
BEGIN:VEVENT\r
DTEND:20260720T200000Z\r
DTSTAMP:20260620T081400Z\r
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

    let output = common::mailsift()
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
    assert!(
        !stderr.contains("Stack backtrace"),
        "user-config error should not include a backtrace: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "usage errors should exit 2, got {stderr}"
    );
}
