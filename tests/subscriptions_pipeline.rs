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

/// `maildir-scan` runs messages on a thread pool, so renewals for one
/// subscription are filed concurrently. The newest must still win.
#[test]
fn renewals_scanned_in_parallel_leave_the_newest() {
    let manifest = manifest_dir();
    let maildir = tempfile::tempdir().expect("maildir tempdir");
    for sub in ["cur", "new", "tmp"] {
        std::fs::create_dir(maildir.path().join(sub)).expect("create maildir subdir");
    }
    // One renewal a day for 50 days; the price counts the days.
    for day in 1..=50 {
        let (month, dom) = if day <= 31 { (1, day) } else { (2, day - 31) };
        let month_name = ["Jan", "Feb"][month - 1];
        let message = format!(
            "From: billing@subscription.fixture.test\r\n\
To: jelmer@example.org\r\n\
Subject: fixture-renewal: {day}\r\n\
Date: {dom} {month_name} 2026 10:00:00 +0000\r\n\
Message-ID: <fixture-renewal-{day}@subscription.fixture.test>\r\n\
Authentication-Results: example.org; dkim=pass header.d=subscription.fixture.test\r\n\
\r\n\
Your subscription has renewed.\r\n"
        );
        std::fs::write(maildir.path().join(format!("cur/{day}.msg")), message)
            .expect("write message");
    }
    let out = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("mailsift")
        .expect("binary built")
        .arg("maildir-scan")
        .arg(maildir.path())
        .arg("--extractors")
        .arg(manifest.join("tests/fixtures/extractors"))
        .arg("--subscriptions-dir")
        .arg(out.path())
        .assert()
        .success();

    let record = filed(out.path());
    assert_eq!(record["price"], 50.0);
    assert_eq!(record["receivedAt"], "2026-02-19T10:00:00Z");
}

/// The milter, a watcher and a one-off scan are separate processes
/// that can all file into the same directory. Each has to wait for
/// whichever of them is in the middle of reading and replacing a
/// record there.
#[test]
fn filing_waits_for_another_process_using_the_directory() {
    let manifest = manifest_dir();
    let out = tempfile::tempdir().expect("tempdir");

    let lock = mailsift::targets::sink::lock_dir(out.path()).expect("lock directory");
    let mut replay = std::process::Command::new(assert_cmd::cargo::cargo_bin("mailsift"))
        .arg("replay")
        .arg(manifest.join("tests/fixtures/eml/subscription-renewal-2026-01.eml"))
        .arg("--extractors")
        .arg(manifest.join("tests/fixtures/extractors"))
        .arg("--subscriptions-dir")
        .arg(out.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn mailsift");

    // Far longer than a replay takes when nothing is in its way.
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(
        replay.try_wait().expect("poll mailsift"),
        None,
        "replay finished while another process held the directory"
    );
    assert!(!out.path().join("fixture-music.json").exists());

    drop(lock);
    assert!(replay.wait().expect("wait for mailsift").success());
    assert_eq!(filed(out.path())["price"], 12.99);
}

/// A message with no `Date:` header is ordered by when it was
/// received instead: the newest `Received:` header.
#[test]
fn message_without_a_date_is_ordered_by_when_it_was_received() {
    let out = tempfile::tempdir().expect("tempdir");
    let mail = tempfile::tempdir().expect("mail tempdir");
    let undated = mail.path().join("undated.eml");
    std::fs::write(
        &undated,
        "Received: by mx.example.org with ESMTPS id abc123;\r\n\
\tSat, 28 Feb 2026 08:30:00 +0000 (UTC)\r\n\
Received: from mail.subscription.fixture.test by relay.example.org;\r\n\
\tSat, 28 Feb 2026 08:29:58 +0000\r\n\
From: billing@subscription.fixture.test\r\n\
To: jelmer@example.org\r\n\
Subject: fixture-renewal: 13.99\r\n\
Message-ID: <fixture-renewal-undated@subscription.fixture.test>\r\n\
Authentication-Results: example.org; dkim=pass header.d=subscription.fixture.test\r\n\
\r\n\
Your subscription has renewed.\r\n",
    )
    .expect("write message");

    // Newer than the January renewal, so it replaces it ...
    run_replay("subscription-renewal-2026-01.eml", out.path());
    run_replay(undated.to_str().unwrap(), out.path());
    let record = filed(out.path());
    assert_eq!(record["price"], 13.99);
    assert_eq!(record["receivedAt"], "2026-02-28T08:30:00Z");

    // ... and the January one, processed again, doesn't take it back.
    run_replay("subscription-renewal-2026-01.eml", out.path());
    assert_eq!(filed(out.path())["price"], 13.99);
}
