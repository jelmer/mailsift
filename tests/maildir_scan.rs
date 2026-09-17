use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;

mod common;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn make_maildir(root: &std::path::Path) {
    fs::create_dir_all(root.join("cur")).unwrap();
    fs::create_dir_all(root.join("new")).unwrap();
    fs::create_dir_all(root.join("tmp")).unwrap();
}

#[test]
fn maildir_scan_processes_flat_maildir() {
    let manifest = manifest_dir();
    let eml = fs::read(manifest.join("tests/fixtures/eml/ics-attachment.eml")).unwrap();
    let extractors = manifest.join("tests/fixtures/extractors");

    let td = tempfile::tempdir().unwrap();
    make_maildir(td.path());
    fs::write(td.path().join("cur/1234.msg"), &eml).unwrap();
    let out = tempfile::tempdir().unwrap();

    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("maildir-scan")
        .arg(td.path())
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .assert()
        .success();

    let path = out.path().join("fixture-ics-1@example.ics");
    assert!(path.exists(), "expected {} to exist", path.display());
    let actual = common::read_event_stable(&path);
    assert!(actual.contains("UID:fixture-ics-1@example.com"), "{actual}");
}

#[test]
fn maildir_scan_recurse_processes_subfolders() {
    let manifest = manifest_dir();
    let eml = fs::read(manifest.join("tests/fixtures/eml/ics-attachment.eml")).unwrap();
    let extractors = manifest.join("tests/fixtures/extractors");

    let td = tempfile::tempdir().unwrap();
    make_maildir(td.path());
    let archive = td.path().join(".archive");
    make_maildir(&archive);
    fs::write(archive.join("cur/9999.msg"), &eml).unwrap();
    let out = tempfile::tempdir().unwrap();

    // Without --recurse: the .archive message is not seen, nothing is filed.
    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("maildir-scan")
        .arg(td.path())
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .assert()
        .success();
    assert!(
        !out.path().join("fixture-ics-1@example.ics").exists(),
        "expected no artifact without --recurse"
    );

    // With --recurse: it is picked up.
    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("maildir-scan")
        .arg(td.path())
        .arg("--recurse")
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .assert()
        .success();
    assert!(
        out.path().join("fixture-ics-1@example.ics").exists(),
        "expected artifact with --recurse"
    );
}

#[test]
fn maildir_scan_rejects_non_maildir() {
    let td = tempfile::tempdir().unwrap();
    let empty_extractors = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("maildir-scan")
        .arg(td.path())
        .arg("--extractors")
        .arg(empty_extractors.path())
        .arg("--events-dir")
        .arg(out.path())
        .output()
        .expect("run mailsift");
    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Either "no extractors" (checked first) or "does not look like a Maildir".
    assert!(
        stderr.contains("no extractors") || stderr.contains("does not look like a Maildir"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn maildir_scan_extractor_flag_selects_one_extractor() {
    let manifest = manifest_dir();
    let extractors = manifest.join("tests/fixtures/extractors");

    let td = tempfile::tempdir().unwrap();
    make_maildir(td.path());
    fs::write(
        td.path().join("cur/1.msg"),
        fs::read(manifest.join("tests/fixtures/eml/ics-attachment.eml")).unwrap(),
    )
    .unwrap();
    fs::write(
        td.path().join("cur/2.msg"),
        fs::read(manifest.join("tests/fixtures/eml/flight-confirmation.eml")).unwrap(),
    )
    .unwrap();

    let out = tempfile::tempdir().unwrap();
    let reservations = tempfile::tempdir().unwrap();
    let tickets = tempfile::tempdir().unwrap();

    // Selecting only the flight extractor leaves the ICS message's
    // artifact unfiled, even though its own extractor would match it.
    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("maildir-scan")
        .arg(td.path())
        .arg("--extractor")
        .arg("fixture-flight")
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .arg("--reservations-dir")
        .arg(reservations.path())
        .arg("--tickets-dir")
        .arg(tickets.path())
        .assert()
        .success();

    assert!(
        !out.path().join("fixture-ics-1@example.ics").exists(),
        "fixture-ics-pass should not have run"
    );
    let filed: Vec<_> = fs::read_dir(reservations.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !filed.is_empty(),
        "expected the flight reservation to be filed, got {filed:?}"
    );
}

#[test]
fn maildir_scan_rejects_unknown_extractor_name() {
    let manifest = manifest_dir();
    let extractors = manifest.join("tests/fixtures/extractors");

    let td = tempfile::tempdir().unwrap();
    make_maildir(td.path());
    let out = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("maildir-scan")
        .arg(td.path())
        .arg("--extractor")
        .arg("no-such-extractor")
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .output()
        .expect("run mailsift");
    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown extractor no-such-extractor"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        stderr.contains("fixture-flight"),
        "expected the available names to be listed: {stderr}"
    );
}

#[test]
fn maildir_scan_prefilter_skips_messages_from_other_senders() {
    let manifest = manifest_dir();
    let extractors = manifest.join("tests/fixtures/extractors");

    let td = tempfile::tempdir().unwrap();
    make_maildir(td.path());
    fs::write(
        td.path().join("cur/1.msg"),
        fs::read(manifest.join("tests/fixtures/eml/lodging-confirmation.eml")).unwrap(),
    )
    .unwrap();
    fs::write(
        td.path().join("cur/2.msg"),
        fs::read(manifest.join("tests/fixtures/eml/flight-confirmation.eml")).unwrap(),
    )
    .unwrap();

    let out = tempfile::tempdir().unwrap();
    let reservations = tempfile::tempdir().unwrap();
    let tickets = tempfile::tempdir().unwrap();

    let output = Command::cargo_bin("mailsift")
        .expect("binary built")
        // The default subscriber styles fields with ANSI escapes,
        // which would sit between the field name and its value.
        .env("NO_COLOR", "1")
        .arg("maildir-scan")
        .arg(td.path())
        .arg("--extractor")
        .arg("fixture-flight")
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(out.path())
        .arg("--reservations-dir")
        .arg(reservations.path())
        .arg("--tickets-dir")
        .arg(tickets.path())
        .output()
        .expect("run mailsift");
    assert!(output.status.success(), "expected success");
    // The subscriber logs to stdout.
    let log = String::from_utf8_lossy(&output.stdout);
    // Only the flight message survives the header prefilter; the
    // lodging one is dropped without its body being read.
    assert!(
        log.contains("header prefilter narrowed the scan scanned=2 matched=1 skipped=1"),
        "expected 1 of 2 messages to survive the prefilter: {log}"
    );
    assert!(
        log.contains("processing messages count=1"),
        "expected only the matching message to be processed: {log}"
    );
}
