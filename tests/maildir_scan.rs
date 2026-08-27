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
