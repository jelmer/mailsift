//! `unsigned_before` lets unsigned mail through only when we received
//! it before the cutoff. The fixture's `Date:` claims 2014, which must
//! not count: only `--received-at` (standing in for IMAP INTERNALDATE
//! or the Maildir delivery time) does.

use std::path::{Path, PathBuf};

mod common;

fn replay(received_at: Option<&str>) -> bool {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let receipts = tempfile::tempdir().expect("tempdir");
    let events = tempfile::tempdir().expect("tempdir");
    let mut cmd = common::mailsift();
    cmd.arg("replay")
        .arg(root.join("tests/fixtures/eml/unsigned-backdated.eml"))
        .arg("--extractors")
        .arg(root.join("tests/fixtures/extractors-unsigned"))
        .arg("--events-dir")
        .arg(events.path())
        .arg("--receipts-dir")
        .arg(receipts.path());
    if let Some(at) = received_at {
        cmd.arg("--received-at").arg(at);
    }
    cmd.assert().success();
    filed(receipts.path())
}

fn filed(root: &Path) -> bool {
    root.join("2026/fixture-shop-ord-42.json").exists()
}

#[test]
fn unsigned_mail_received_before_the_cutoff_is_accepted() {
    assert!(replay(Some("2015-06-01")));
}

#[test]
fn unsigned_mail_received_after_the_cutoff_is_refused() {
    assert!(!replay(Some("2019-01-01T00:00:00Z")));
}

#[test]
fn a_backdated_date_header_does_not_count_as_received() {
    assert!(!replay(None));
}
