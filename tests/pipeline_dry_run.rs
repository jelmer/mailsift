//! `--dry-run` on the CLI must skip every target-side write while
//! still exercising extractors and reporting what would have been
//! filed. This test drives `replay --dry-run` against a fixture that
//! is known to produce a parcel and asserts (a) the "would extract"
//! log line fires and (b) nothing lands in the artifact directories.

use std::path::PathBuf;

use assert_cmd::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn dry_run_reports_counts_and_writes_nothing() {
    let manifest = manifest_dir();
    let extractors = manifest.join("tests/fixtures/extractors");

    let events = tempfile::tempdir().expect("events tempdir");
    let parcels = tempfile::tempdir().expect("parcels tempdir");

    let assert = Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("replay")
        .arg(manifest.join("tests/fixtures/eml/parcel-delivered.eml"))
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(events.path())
        .arg("--parcels-dir")
        .arg(parcels.path())
        .arg("--dry-run")
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("[dry-run] would extract"),
        "expected dry-run rollup line in stdout, got:\n{stdout}"
    );
    assert!(
        stdout.contains("parcel"),
        "expected the rollup line to mention the parcel count, got:\n{stdout}"
    );

    let parcel_entries: Vec<_> = std::fs::read_dir(parcels.path())
        .expect("read parcels dir")
        .collect();
    assert!(
        parcel_entries.is_empty(),
        "dry-run must not write parcels; found {} entries",
        parcel_entries.len()
    );
    let event_entries: Vec<_> = std::fs::read_dir(events.path())
        .expect("read events dir")
        .collect();
    assert!(
        event_entries.is_empty(),
        "dry-run must not write events; found {} entries",
        event_entries.len()
    );
}
