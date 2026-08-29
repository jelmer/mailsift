//! Read-only HTTP dashboard for the artifact directories.
//!
//! Scans the configured `bills_dir`, `parcels_dir`, `receipts_dir`,
//! `subscriptions_dir`, `events_dir`, and `tickets_dir` on each request
//! and renders a small HTML view of what's there. JSON views and raw
//! file downloads are exposed alongside so the same server can be used
//! as a data source for other tools.
//!
//! Directory scans happen per request rather than being cached: the
//! artifact set is small (hundreds of files at most for a personal
//! inbox) and the extractor daemons write to these dirs while the web
//! server runs, so a rescan avoids showing stale data.

mod error;
mod feed;
mod handlers;
mod render;
mod scan;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;
use tracing::info;

use crate::config::Config;

/// How the web server should listen. Mirrors the milter's
/// `--socket unix:/path` / `tcp:host:port` convention.
pub enum Listen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl Listen {
    /// Parse `unix:/path`, or fall back to a TCP `SocketAddr`.
    pub fn parse(spec: &str) -> Result<Self> {
        if let Some(path) = spec.strip_prefix("unix:") {
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        let addr: SocketAddr = spec
            .parse()
            .with_context(|| format!("parsing listen spec {spec:?}"))?;
        Ok(Self::Tcp(addr))
    }
}

/// Shared handler state: the mailsift config plus URL-generation
/// context. Every artifact directory the UI reads from lives on the
/// [`Config`]; there's no separate copy.
#[derive(Clone, Default)]
pub struct AppState {
    pub config: Arc<Config>,
    /// URL prefix under which the app is mounted, without a trailing
    /// slash. Empty when served at the site root. Prepended to every
    /// generated URL so links work behind a reverse proxy that only
    /// forwards a sub-path (e.g. `location /mailsift/`).
    pub base_path: String,
}

impl AppState {
    /// Prefix a root-relative path with the configured base. `p` must
    /// start with `/`.
    fn url(&self, p: &str) -> String {
        if self.base_path.is_empty() {
            p.to_string()
        } else {
            format!("{}{}", self.base_path, p)
        }
    }

    fn events_dir(&self) -> Option<&Path> {
        self.config.events_dir.as_deref()
    }
    fn bills_dir(&self) -> Option<&Path> {
        self.config.bills_dir.as_deref()
    }
    fn parcels_dir(&self) -> Option<&Path> {
        self.config.parcels_dir.as_deref()
    }
    fn subscriptions_dir(&self) -> Option<&Path> {
        self.config.subscriptions_dir.as_deref()
    }

    /// Receipts have three possible sinks; only the local one is
    /// browsable. `Config::validate` guarantees at most one is set.
    fn receipts_dir(&self) -> Option<&Path> {
        if self.config.receipts_webdav.is_some() || self.config.receipts_forward.is_some() {
            None
        } else {
            self.config.receipts_dir.as_deref()
        }
    }

    /// Tickets can go to WebDAV; only the local dir is browsable.
    fn tickets_dir(&self) -> Option<&Path> {
        if self.config.tickets_webdav.is_some() {
            None
        } else {
            self.config.tickets_dir.as_deref()
        }
    }
}

/// Normalise a user-supplied `--base-path` value:
/// - empty or `/` yields `""` (no prefix).
/// - otherwise, ensure a leading `/` and drop any trailing `/`.
pub fn normalise_base_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return String::new();
    }
    let mut s = trimmed.trim_end_matches('/').to_string();
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    s
}

/// Default permissions for a unix listen socket. 0666 so a reverse
/// proxy running as a different user can connect; the socket path is
/// typically under `$HOME` so restricting further is optional.
pub const DEFAULT_SOCKET_MODE: u32 = 0o666;

/// Run the web server until it errors or the process is signalled.
pub async fn serve(
    listen: Listen,
    config: Arc<Config>,
    base_path: String,
    socket_mode: Option<u32>,
) -> Result<()> {
    let state = Arc::new(AppState { config, base_path });
    let app = router(state);

    match listen {
        Listen::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {addr}"))?;
            let bound = listener.local_addr().unwrap_or(addr);
            info!(%bound, "web server listening");
            axum::serve(listener, app)
                .await
                .context("axum serve failed")?;
        }
        Listen::Unix(path) => {
            serve_unix(&path, app, socket_mode.unwrap_or(DEFAULT_SOCKET_MODE)).await?
        }
    }
    Ok(())
}

/// Accept-loop for a unix socket. axum 0.7's built-in `serve` only
/// takes a `TcpListener`, so we drive hyper directly. Each accepted
/// connection gets its own tokio task and a fresh `Router` clone; the
/// router is cheap to clone (it's an `Arc` internally).
async fn serve_unix(path: &Path, app: Router, mode: u32) -> Result<()> {
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use tower_service::Service;

    // Best-effort remove of a stale socket; systemd's ExecStartPre
    // does this too but running standalone benefits.
    let _ = fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("binding {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;
    }
    info!(path = %path.display(), mode = format!("{mode:o}"), "web server listening");

    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .context("accepting unix connection")?;
        let io = TokioIo::new(stream);
        let app = app.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: hyper::Request<Incoming>| {
                let mut app = app.clone();
                let req = req.map(axum::body::Body::new);
                async move { app.call(req).await }
            });
            if let Err(err) = Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                tracing::warn!(error = %err, "unix connection failed");
            }
        });
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(feed::index))
        .route("/all", get(feed::list_all))
        .route("/events", get(handlers::list_events))
        .route("/events/:name", get(handlers::get_event))
        .route("/bills", get(handlers::list_bills))
        .route("/bills/:year/:name", get(handlers::get_bill))
        .route("/parcels", get(handlers::list_parcels))
        .route("/parcels/:name", get(handlers::get_parcel))
        .route("/receipts", get(handlers::list_receipts))
        .route("/receipts/:year/:name", get(handlers::get_receipt))
        .route("/subscriptions", get(handlers::list_subscriptions))
        .route("/subscriptions/:name", get(handlers::get_subscription))
        .route("/tickets", get(handlers::list_tickets))
        .route("/tickets/:year/:name", get(handlers::get_ticket))
        .route("/api/bills.json", get(handlers::api_bills))
        .route("/api/parcels.json", get(handlers::api_parcels))
        .route("/api/receipts.json", get(handlers::api_receipts))
        .route("/api/subscriptions.json", get(handlers::api_subscriptions))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    fn fixture() -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().unwrap();
        let bills = tmp.path().join("bills/2026");
        let parcels = tmp.path().join("parcels");
        let events = tmp.path().join("events");
        fs::create_dir_all(&bills).unwrap();
        fs::create_dir_all(&parcels).unwrap();
        fs::create_dir_all(&events).unwrap();
        fs::write(
            bills.join("acme-INV1.json"),
            br#"{"payee":"Acme","invoiceNumber":"INV1","dueDate":"2026-05-01"}"#,
        )
        .unwrap();
        fs::write(
            parcels.join("TQ123GB.json"),
            br#"{"trackingNumber":"TQ123GB","deliveryStatus":"OutForDelivery"}"#,
        )
        .unwrap();
        fs::write(
            events.join("flight-1.ics"),
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:flight-1\r\nSUMMARY:Flight\r\n\
              DTSTART:20260201T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        )
        .unwrap();
        let config = Config {
            events_dir: Some(events),
            bills_dir: Some(tmp.path().join("bills")),
            parcels_dir: Some(parcels),
            ..Config::default()
        };
        (tmp, config)
    }

    fn state_with(config: Config, base_path: &str) -> Arc<AppState> {
        Arc::new(AppState {
            config: Arc::new(config),
            base_path: base_path.into(),
        })
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn homepage_shows_feed_sections() {
        // Fixture bill is due 2026-05-01 (past by 2026-08-27), event is
        // 2026-02-01 (also past). Both should land in Recent.
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Recent"), "no Recent section: {body}");
        assert!(body.contains("Acme"), "Acme bill missing: {body}");
        assert!(body.contains("Flight"), "event missing: {body}");
    }

    #[tokio::test]
    async fn feed_prefers_received_at_over_due_date() {
        // Two bills with dueDate=today but different receivedAt. Feed
        // should sort them by receivedAt.
        let tmp = tempfile::tempdir().unwrap();
        let bills = tmp.path().join("bills/2026");
        fs::create_dir_all(&bills).unwrap();
        fs::write(
            bills.join("acme-A.json"),
            br#"{"payee":"Acme","invoiceNumber":"A","dueDate":"2027-01-01",
                 "receivedAt":"2025-06-01T00:00:00Z"}"#,
        )
        .unwrap();
        fs::write(
            bills.join("acme-B.json"),
            br#"{"payee":"Acme","invoiceNumber":"B","dueDate":"2027-01-01",
                 "receivedAt":"2026-05-01T00:00:00Z"}"#,
        )
        .unwrap();
        let config = Config {
            bills_dir: Some(tmp.path().join("bills")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/").await;
        let pos_a = body.find("acme-A").expect("acme A missing");
        let pos_b = body.find("acme-B").expect("acme B missing");
        // Newer receivedAt (B, 2026) should appear before older (A, 2025)
        // in the Recent list.
        assert!(
            pos_b < pos_a,
            "expected B before A (newer receivedAt first); body: {body}"
        );
    }

    #[tokio::test]
    async fn list_all_route_serves() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/all").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("All artifacts"));
    }

    #[tokio::test]
    async fn footer_contains_copyright_and_repo_link() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/").await;
        assert!(body.contains("github.com/jelmer/mailsift"), "no repo link");
        assert!(body.contains("2025-2026"), "no copyright year");
        assert!(body.contains("jelmer@jelmer.uk"), "no author email");
    }

    #[tokio::test]
    async fn bill_row_renders_vendor_open_link() {
        let (tmp, mut config) = fixture();
        // Overwrite the fixture bill with one that has a `url`.
        let bill = tmp.path().join("bills/2026/acme-INV1.json");
        fs::write(
            &bill,
            br#"{"payee":"Acme","invoiceNumber":"INV1","dueDate":"2026-05-01",
                 "url":"https://acme.example/invoice/INV1"}"#,
        )
        .unwrap();
        config.bills_dir = Some(tmp.path().join("bills"));
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/bills").await;
        assert!(
            body.contains("https://acme.example/invoice/INV1"),
            "vendor URL missing: {body}"
        );
        assert!(
            body.contains("rel=\"noopener noreferrer\""),
            "external link rel missing"
        );
    }

    #[tokio::test]
    async fn overview_renders() {
        let (tmp, config) = fixture();
        let bills_dir = config.bills_dir.clone().expect("fixture has bills");
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Overview"), "body: {body}");
        assert!(body.contains("bills"));
        // Local filesystem paths must not leak into the UI.
        assert!(
            !body.contains(&bills_dir.display().to_string()),
            "overview leaked bills_dir path"
        );
        assert!(
            !body.contains(&tmp.path().display().to_string()),
            "overview leaked tmpdir path"
        );
    }

    #[tokio::test]
    async fn unconfigured_kinds_hidden_from_navbar_and_overview() {
        // Fixture only configures events/bills/parcels; the other three
        // (receipts, subscriptions, tickets) should be omitted entirely
        // rather than showing "not configured".
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("not configured"), "body: {body}");
        assert!(!body.contains(">receipts<"), "receipts should be hidden");
        assert!(
            !body.contains(">subscriptions<"),
            "subscriptions should be hidden"
        );
        assert!(!body.contains(">tickets<"), "tickets should be hidden");
        // The configured ones should still show.
        assert!(body.contains(">events<"));
        assert!(body.contains(">bills<"));
        assert!(body.contains(">parcels<"));
    }

    #[tokio::test]
    async fn bills_list_shows_row() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/bills").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Acme"), "body: {body}");
        assert!(body.contains("INV1"));
    }

    #[tokio::test]
    async fn parcels_json_api() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/api/parcels.json").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["trackingNumber"], "TQ123GB");
    }

    #[tokio::test]
    async fn events_download_serves_ics() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/events/flight-1.ics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("BEGIN:VCALENDAR"));
    }

    #[tokio::test]
    async fn missing_file_returns_404() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, _) = get(&app, "/events/does-not-exist.ics").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn traversal_returns_400() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        // %2F is a path separator; axum decodes it into the segment, which
        // safe_segment then rejects.
        let (status, _) = get(&app, "/parcels/..%2Fescape.json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unconfigured_kind_returns_404() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, _) = get(&app, "/receipts").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn receipts_dir_hidden_when_webdav_set() {
        // Config::validate forbids receipts_dir + receipts_webdav in the
        // same config, so the interesting case is: webdav configured,
        // no local dir. The web UI has nothing local to serve.
        let cfg: Config = toml::from_str(
            r#"
[receipts_webdav]
url = "https://example.org/dav/"
"#,
        )
        .unwrap();
        assert!(state_with(cfg, "").receipts_dir().is_none());
    }

    #[test]
    fn receipts_dir_hidden_when_forward_set() {
        let cfg: Config = toml::from_str(
            r#"
[receipts_forward]
from = "mailsift@example.org"
to = ["archive@example.org"]
sendmail = "/usr/sbin/sendmail"
"#,
        )
        .unwrap();
        assert!(state_with(cfg, "").receipts_dir().is_none());
    }

    #[test]
    fn tickets_dir_hidden_when_webdav_set() {
        let cfg: Config = toml::from_str(
            r#"
[tickets_webdav]
url = "https://example.org/dav/tickets/"
"#,
        )
        .unwrap();
        assert!(state_with(cfg, "").tickets_dir().is_none());
    }

    #[test]
    fn local_dirs_readable_from_state() {
        let cfg: Config = toml::from_str(
            r#"
bills_dir = "/var/mailsift/bills"
parcels_dir = "/var/mailsift/parcels"
events_dir = "/var/mailsift/events"
subscriptions_dir = "/var/mailsift/subs"
receipts_dir = "/var/mailsift/receipts"
tickets_dir = "/var/mailsift/tickets"
"#,
        )
        .unwrap();
        let s = state_with(cfg, "");
        assert_eq!(s.bills_dir(), Some(Path::new("/var/mailsift/bills")));
        assert_eq!(s.parcels_dir(), Some(Path::new("/var/mailsift/parcels")));
        assert_eq!(s.events_dir(), Some(Path::new("/var/mailsift/events")));
        assert_eq!(s.subscriptions_dir(), Some(Path::new("/var/mailsift/subs")));
        assert_eq!(s.receipts_dir(), Some(Path::new("/var/mailsift/receipts")));
        assert_eq!(s.tickets_dir(), Some(Path::new("/var/mailsift/tickets")));
    }

    #[test]
    fn normalise_base_path_variants() {
        assert_eq!(normalise_base_path(""), "");
        assert_eq!(normalise_base_path("/"), "");
        assert_eq!(normalise_base_path("mailsift"), "/mailsift");
        assert_eq!(normalise_base_path("/mailsift"), "/mailsift");
        assert_eq!(normalise_base_path("/mailsift/"), "/mailsift");
        assert_eq!(normalise_base_path("  /mailsift/  "), "/mailsift");
    }

    #[tokio::test]
    async fn base_path_prefixes_generated_urls() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, "/mailsift"));
        let (status, body) = get(&app, "/bills").await;
        assert_eq!(status, StatusCode::OK);
        // Every generated link should include the prefix.
        assert!(
            body.contains("href=\"/mailsift/bills/"),
            "expected /mailsift/bills/ links; body: {body}"
        );
        assert!(
            body.contains("href=\"/mailsift/events\""),
            "expected header link /mailsift/events; body: {body}"
        );
        // And no unprefixed root-relative artifact links.
        assert!(
            !body.contains("href=\"/bills/"),
            "body should not carry unprefixed /bills/ links"
        );
    }
}
