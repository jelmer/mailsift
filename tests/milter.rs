use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use indymilter_test::*;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const LOCALHOST: (Ipv4Addr, u16) = (Ipv4Addr::LOCALHOST, 0);

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Split an RFC822 blob on the header/body boundary and yield unfolded
/// `(name, value)` header pairs plus the raw body bytes, matching the
/// shape the milter's `on_header`/`on_body` callbacks expect.
fn split_headers_and_body(eml: &str) -> (Vec<(String, String)>, &str) {
    let bytes = eml.as_bytes();
    let (hdr_end, body_start) = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| {
            bytes
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, i + 2))
        })
        .expect("eml separator");
    let headers_blob = &eml[..hdr_end];
    let body = &eml[body_start..];

    let mut headers = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in headers_blob.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, v)) = current.as_mut() {
                v.push(' ');
                v.push_str(line.trim_start());
            }
            continue;
        }
        if let Some(prev) = current.take() {
            headers.push(prev);
        }
        let (n, v) = line.split_once(':').expect("header colon");
        current = Some((n.to_string(), v.trim_start().to_string()));
    }
    if let Some(prev) = current.take() {
        headers.push(prev);
    }
    (headers, body)
}

#[tokio::test]
async fn milter_files_event_from_ics_attachment() {
    let _ = tracing_subscriber::fmt::try_init();

    let manifest = manifest_dir();
    let eml = std::fs::read_to_string(manifest.join("tests/fixtures/eml/ics-attachment.eml"))
        .expect("read eml");

    let (headers, body) = split_headers_and_body(&eml);

    let out = TempDir::new().unwrap();
    let sink = mailsift::targets::EventSinkKind::LocalDir(out.path().to_path_buf());
    let extractors_dirs = vec![manifest.join("tests/fixtures/extractors")];
    let extractors = mailsift::extractor::discover(&extractors_dirs).expect("discover extractors");
    let config = mailsift::milter::MilterConfig {
        extractors: Arc::new(extractors),
        targets: mailsift::pipeline::OwnedTargets {
            event_sink: Arc::new(sink),
            bills_dir: None,
            parcels_dir: None,
            subscriptions_dir: None,
            reservations_dir: None,
            receipts: None,
            tickets: None,
            firefly: None,
            trackers: None,
            trusted_forwarders: vec![],
            recorder: mailsift::stats::Recorder::Disabled,
            seen: None,
        },
        deadline: Duration::from_secs(10),
    };

    let listener = TcpListener::bind(LOCALHOST).await.unwrap();
    let milter_addr = listener.local_addr().unwrap();
    let callbacks = mailsift::milter::build_callbacks(Arc::new(config));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let milter = tokio::spawn(indymilter::run(
        listener,
        callbacks,
        Default::default(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let mut conn = TestConnection::configure()
        .read_timeout(Duration::from_secs(10))
        .write_timeout(Duration::from_secs(10))
        .open_tcp(milter_addr)
        .await
        .unwrap();

    let status = conn
        .connect("client.example.org", [127, 0, 0, 1])
        .await
        .unwrap();
    assert_eq!(status, Status::Continue);

    let status = conn.helo("mail.example.org").await.unwrap();
    assert_eq!(status, Status::Continue);

    let status = conn.mail(["<bookings@example.com>"]).await.unwrap();
    assert_eq!(status, Status::Continue);

    let status = conn.rcpt(["<jelmer@example.org>"]).await.unwrap();
    assert_eq!(status, Status::Continue);

    for (name, value) in &headers {
        let status = conn.header(name.as_str(), value.as_str()).await.unwrap();
        assert_eq!(status, Status::Continue);
    }

    let status = conn.eoh().await.unwrap();
    assert_eq!(status, Status::Continue);

    let status = conn.body(body.as_bytes().to_vec()).await.unwrap();
    assert_eq!(status, Status::Continue);

    let (_actions, status) = conn.eom().await.unwrap();
    assert_eq!(status, Status::Continue);

    conn.close().await.unwrap();
    shutdown_tx.send(()).unwrap();
    milter.await.unwrap().unwrap();

    let expected = out.path().join("fixture-ics-1@example.ics");
    assert!(
        expected.exists(),
        "expected event file at {} not found",
        expected.display()
    );
}

/// Feed a Gmail-style inline forward from a trusted forwarder through
/// the milter and check that the fixture `dkim-required` extractor
/// still runs against the inner mail. The inner has no
/// `Authentication-Results` header, so this pins the trusted-forwarder
/// bypass on the milter entry point in addition to the `replay` path
/// covered by `tests/pipeline_inline_forward.rs`.
#[tokio::test]
async fn milter_inline_forward_from_trusted_sender_reaches_dkim_required_extractor() {
    let _ = tracing_subscriber::fmt::try_init();

    let manifest = manifest_dir();

    let eml = "\
From: Joe <joe@example.com>\r\n\
To: jelmer@example.org\r\n\
Subject: Fwd: fixture-dkim inline forward\r\n\
Date: Sat, 23 Aug 2025 21:00:00 +0200\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Just wanted you to see this.\r\n\
\r\n\
---------- Forwarded message ---------\r\n\
From: Fixture <sender@dkim-required.fixture.test>\r\n\
Date: Sat, 23 Aug 2025 at 20:57\r\n\
Subject: fixture-dkim inline forward\r\n\
To: Friend <friend@example.com>\r\n\
\r\n\
hello from an unsigned inner mail\r\n\
"
    .to_string();

    let (headers, body) = split_headers_and_body(&eml);

    let out = TempDir::new().unwrap();
    let sink = mailsift::targets::EventSinkKind::LocalDir(out.path().to_path_buf());
    let extractors_dirs = vec![manifest.join("tests/fixtures/extractors")];
    let extractors = mailsift::extractor::discover(&extractors_dirs).expect("discover extractors");
    let config = mailsift::milter::MilterConfig {
        extractors: Arc::new(extractors),
        targets: mailsift::pipeline::OwnedTargets {
            event_sink: Arc::new(sink),
            bills_dir: None,
            parcels_dir: None,
            subscriptions_dir: None,
            reservations_dir: None,
            receipts: None,
            tickets: None,
            firefly: None,
            trackers: None,
            trusted_forwarders: vec!["joe@example.com".to_string()],
            recorder: mailsift::stats::Recorder::Disabled,
            seen: None,
        },
        deadline: Duration::from_secs(10),
    };

    let listener = TcpListener::bind(LOCALHOST).await.unwrap();
    let milter_addr = listener.local_addr().unwrap();
    let callbacks = mailsift::milter::build_callbacks(Arc::new(config));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let milter = tokio::spawn(indymilter::run(
        listener,
        callbacks,
        Default::default(),
        async move {
            let _ = shutdown_rx.await;
        },
    ));

    let mut conn = TestConnection::configure()
        .read_timeout(Duration::from_secs(10))
        .write_timeout(Duration::from_secs(10))
        .open_tcp(milter_addr)
        .await
        .unwrap();

    assert_eq!(
        conn.connect("client.example.org", [127, 0, 0, 1])
            .await
            .unwrap(),
        Status::Continue
    );
    assert_eq!(
        conn.helo("mail.example.org").await.unwrap(),
        Status::Continue
    );
    assert_eq!(
        conn.mail(["<joe@example.com>"]).await.unwrap(),
        Status::Continue
    );
    assert_eq!(
        conn.rcpt(["<jelmer@example.org>"]).await.unwrap(),
        Status::Continue
    );

    for (name, value) in &headers {
        assert_eq!(
            conn.header(name.as_str(), value.as_str()).await.unwrap(),
            Status::Continue
        );
    }
    assert_eq!(conn.eoh().await.unwrap(), Status::Continue);
    assert_eq!(
        conn.body(body.as_bytes().to_vec()).await.unwrap(),
        Status::Continue
    );
    let (_actions, status) = conn.eom().await.unwrap();
    assert_eq!(status, Status::Continue);

    conn.close().await.unwrap();
    shutdown_tx.send(()).unwrap();
    milter.await.unwrap().unwrap();

    let expected = out.path().join("dkim-marker@example.ics");
    assert!(
        expected.exists(),
        "expected fixture extractor artifact at {} not found; \
         milter path failed to unwrap the inline forward or bypass require_dkim",
        expected.display()
    );
}
