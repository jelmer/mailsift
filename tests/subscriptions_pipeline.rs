//! Pipeline-level test for the `subscription` artifact path.
//!
//! A subscription is one record that successive renewal mails refresh.
//! A mailbox isn't processed in date order, so the record has to end
//! up reflecting the newest mail whichever one is replayed last.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_replay(eml: &str, subscriptions_dir: &Path) {
    let manifest = manifest_dir();
    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("replay")
        .arg(manifest.join("tests/fixtures/eml").join(eml))
        .arg("--extractors")
        .arg(manifest.join("tests/fixtures/extractors"))
        .arg("--subscriptions-dir")
        .arg(subscriptions_dir)
        .assert()
        .success();
}

fn filed(subscriptions_dir: &Path) -> serde_json::Value {
    let body = std::fs::read_to_string(subscriptions_dir.join("fixture-music.json"))
        .expect("read subscription");
    serde_json::from_str(&body).expect("subscription is valid JSON")
}

#[test]
fn renewals_in_date_order_leave_the_newest() {
    let out = tempfile::tempdir().expect("tempdir");

    run_replay("subscription-renewal-2025-10.eml", out.path());
    run_replay("subscription-renewal-2026-01.eml", out.path());

    let record = filed(out.path());
    assert_eq!(record["price"], 12.99);
    assert_eq!(record["receivedAt"], "2026-01-28T10:00:00Z");
}

/// A rescan of an older stretch of the mailbox hands us the October
/// renewal after the January one. It must not roll the record back.
#[test]
fn renewals_out_of_date_order_leave_the_newest() {
    let out = tempfile::tempdir().expect("tempdir");

    run_replay("subscription-renewal-2026-01.eml", out.path());
    run_replay("subscription-renewal-2025-10.eml", out.path());

    let record = filed(out.path());
    assert_eq!(record["price"], 12.99);
    assert_eq!(record["receivedAt"], "2026-01-28T10:00:00Z");
}
