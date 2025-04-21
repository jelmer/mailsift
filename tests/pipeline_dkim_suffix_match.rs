//! Pipeline-level tests for DKIM suffix-match routing.
//!
//! When a manifest declares a signing-domain suffix like
//! `.tenant.fixture.test` instead of a literal domain list, the
//! pipeline must (a) run the extractor against any mail whose passing
//! DKIM signature ends in that suffix, and (b) refuse to run it
//! against a lookalike like `evil-tenant.fixture.test` that doesn't
//! satisfy the literal-dot boundary. Driven by a synthetic fixture
//! extractor so the test doesn't depend on any specific real
//! extractor being present.

use std::path::PathBuf;

use assert_cmd::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn matching_dkim_suffix_runs_extractor() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/dkim-suffix-signed.eml");
    let extractors = manifest.join("tests/fixtures/extractors");

    let receipts = tempfile::tempdir().expect("tempdir");
    let events = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("replay")
        .arg(&eml)
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(events.path())
        .arg("--receipts-dir")
        .arg(receipts.path())
        .assert()
        .success();

    let expected = receipts.path().join("2026/fixture-shop-ord-42.json");
    assert!(
        expected.exists(),
        "expected receipt at {} not found",
        expected.display()
    );
}

#[test]
fn lookalike_dkim_domain_does_not_match_suffix() {
    let manifest = manifest_dir();
    let eml = manifest.join("tests/fixtures/eml/dkim-suffix-lookalike.eml");
    let extractors = manifest.join("tests/fixtures/extractors");

    let receipts = tempfile::tempdir().expect("tempdir");
    let events = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("replay")
        .arg(&eml)
        .arg("--extractors")
        .arg(&extractors)
        .arg("--events-dir")
        .arg(events.path())
        .arg("--receipts-dir")
        .arg(receipts.path())
        .assert()
        .success();

    // The mail's DKIM signature is from `evil-tenant.fixture.test`,
    // which doesn't satisfy the `.tenant.fixture.test` suffix match
    // (the suffix explicitly requires a literal dot before the parent
    // zone). The pipeline should run nothing.
    let entries: Vec<_> = walk(receipts.path())
        .into_iter()
        .filter(|p| p.is_file())
        .collect();
    assert!(entries.is_empty(), "expected no receipts; got: {entries:?}");
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}
