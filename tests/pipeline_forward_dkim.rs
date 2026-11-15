//! A forward from `trusted_forwarders` stands in for the vendor's DKIM
//! signature, so the outer message itself has to be authenticated as
//! coming from the forwarder; a bare `From:` header is trivially forged.

use std::path::{Path, PathBuf};

use mailsift::pipeline::{self, DkimPolicy, PipelineTargets};
use mailsift::stats::Recorder;
use mailsift::targets::EventSinkKind;

fn inline_forward(outer_auth_results: Option<&str>) -> Vec<u8> {
    let mut eml = String::new();
    if let Some(ar) = outer_auth_results {
        eml.push_str(&format!("Authentication-Results: {ar}\r\n"));
    }
    eml.push_str(
        "\
From: Ada Example <ada@forwarder.example>\r\n\
To: traveller@example.org\r\n\
Subject: Fwd: fixture-dkim inline forward\r\n\
Date: Sat, 23 Aug 2025 21:00:00 +0200\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
See below.\r\n\
\r\n\
---------- Forwarded message ---------\r\n\
From: Fixture <sender@dkim-required.fixture.test>\r\n\
Date: Sat, 23 Aug 2025 at 20:57\r\n\
Subject: fixture-dkim inline forward\r\n\
To: Ada Example <ada@forwarder.example>\r\n\
\r\n\
hello from an unsigned inner mail\r\n",
    );
    eml.into_bytes()
}

/// Run the fixture extractors over `raw` and report whether the
/// `require_dkim` fixture extractor filed its event.
fn dkim_extractor_fired(raw: &[u8], policy: DkimPolicy) -> bool {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let extractors = mailsift::extractor::discover(&[manifest.join("tests/fixtures/extractors")])
        .expect("discover extractors");
    let out = tempfile::tempdir().expect("tempdir");
    let sink = EventSinkKind::LocalDir(out.path().to_path_buf());
    let trusted = vec!["ada@forwarder.example".to_string()];
    pipeline::run(
        raw,
        "test",
        &extractors,
        PipelineTargets {
            event_sink: &sink,
            bills_dir: None,
            parcels_dir: None,
            subscriptions_dir: None,
            reservations_dir: None,
            receipts: None,
            tickets: None,
            firefly: None,
            trackers: None,
            trusted_forwarders: &trusted,
            recorder: &Recorder::Disabled,
            seen: None,
        },
        policy,
        false,
        None,
    )
    .expect("pipeline run");
    event_exists(out.path())
}

fn event_exists(dir: &Path) -> bool {
    dir.join("dkim-marker@example.ics").exists()
}

const ENFORCE: DkimPolicy = DkimPolicy::Enforce { received_at: None };

#[test]
fn forward_signed_by_forwarder_domain_bypasses_require_dkim() {
    let raw = inline_forward(Some("mx.example.org; dkim=pass header.d=forwarder.example"));
    assert!(dkim_extractor_fired(&raw, ENFORCE));
}

#[test]
fn forged_forward_without_dkim_does_not_bypass_require_dkim() {
    let raw = inline_forward(None);
    assert!(!dkim_extractor_fired(&raw, ENFORCE));
}

#[test]
fn forward_with_failing_dkim_does_not_bypass_require_dkim() {
    let raw = inline_forward(Some("mx.example.org; dkim=fail header.d=forwarder.example"));
    assert!(!dkim_extractor_fired(&raw, ENFORCE));
}

#[test]
fn forward_signed_by_another_domain_does_not_bypass_require_dkim() {
    let raw = inline_forward(Some("mx.example.org; dkim=pass header.d=attacker.example"));
    assert!(!dkim_extractor_fired(&raw, ENFORCE));
}

/// The milter runs before our MTA has authenticated anything and
/// enforces no `require_dkim` at all, so forwards keep unwrapping there.
#[test]
fn unsigned_forward_still_unwraps_when_dkim_is_skipped() {
    let raw = inline_forward(None);
    assert!(dkim_extractor_fired(&raw, DkimPolicy::Skip));
}
