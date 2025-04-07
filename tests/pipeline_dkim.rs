//! Pipeline-level test for `require_dkim` enforcement.
//!
//! When an extractor's manifest declares `require_dkim`, the pipeline
//! must refuse to run it against a message that lacks a passing
//! `Authentication-Results` entry for the listed domain — even if the
//! mail's `From` address and subject pattern would otherwise look
//! convincing. Driven by a synthetic fixture extractor so the test
//! doesn't depend on any specific real extractor being present.

use std::path::PathBuf;

use assert_cmd::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn spoofed_message_produces_no_artifacts() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/dkim-required-spoofed.eml");
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

    let entries: Vec<_> = std::fs::read_dir(out.path())
        .expect("read events dir")
        .collect::<Result<_, _>>()
        .expect("read entries");
    assert!(
        entries.is_empty(),
        "expected no events; got: {:?}",
        entries.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

/// Sanity check: the matching signed mail does get the extractor run
/// against it. Without this the negative test could pass for the
/// wrong reason (e.g. the fixture extractor never matching the
/// message in the first place).
#[test]
fn signed_message_produces_artifact() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/dkim-required-signed.eml");
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

    assert!(
        out.path().join("dkim-marker@example.ics").exists(),
        "expected dkim-marker event on the legitimately signed message"
    );
}
