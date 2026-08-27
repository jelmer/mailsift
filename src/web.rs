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

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;
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
        .route("/", get(index))
        .route("/all", get(list_all))
        .route("/events", get(list_events))
        .route("/events/:name", get(get_event))
        .route("/bills", get(list_bills))
        .route("/bills/:year/:name", get(get_bill))
        .route("/parcels", get(list_parcels))
        .route("/parcels/:name", get(get_parcel))
        .route("/receipts", get(list_receipts))
        .route("/receipts/:year/:name", get(get_receipt))
        .route("/subscriptions", get(list_subscriptions))
        .route("/subscriptions/:name", get(get_subscription))
        .route("/tickets", get(list_tickets))
        .route("/tickets/:year/:name", get(get_ticket))
        .route("/api/bills.json", get(api_bills))
        .route("/api/parcels.json", get(api_parcels))
        .route("/api/receipts.json", get(api_receipts))
        .route("/api/subscriptions.json", get(api_subscriptions))
        .with_state(state)
}

/// Wraps `anyhow::Error` with an HTTP status. Bad requests and
/// missing resources get their own status; everything else falls
/// through as `500`.
struct AppError {
    status: StatusCode,
    err: anyhow::Error,
}

impl AppError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            err: anyhow::anyhow!(msg.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Log the full detail server-side; render a generic reason to
        // the client so we don't leak filesystem paths or config keys.
        if self.status.is_server_error() {
            tracing::warn!(error = format!("{:#}", self.err), "web request failed");
        }
        let reason = self.status.canonical_reason().unwrap_or("Error");
        let state = AppState::default();
        (
            self.status,
            Html(page(&state, reason, &format!("<p>{}</p>", esc(reason)))),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err: err.into(),
        }
    }
}

/// Map a filesystem error to a status code: NotFound -> 404, else 500.
fn read_status(path: &Path, err: io::Error) -> AppError {
    let status = if err.kind() == io::ErrorKind::NotFound {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    AppError {
        status,
        err: anyhow::Error::from(err).context(format!("reading {}", path.display())),
    }
}

const CSS: &str = r#"
body { font-family: system-ui, sans-serif; margin: 0; color: #222; background: #fafafa; }
header { background: #2a3f5f; color: #fff; padding: 0.75rem 1.25rem; }
header a { color: #fff; text-decoration: none; margin-right: 1rem; font-weight: 500; }
header a:hover { text-decoration: underline; }
main { max-width: 960px; margin: 1.5rem auto; padding: 0 1.25rem; }
h1 { margin-top: 0; }
table { border-collapse: collapse; width: 100%; background: #fff; }
th, td { padding: 0.5rem 0.75rem; text-align: left; border-bottom: 1px solid #eee; vertical-align: top; }
th { background: #f2f4f8; font-weight: 600; }
tr:hover td { background: #fbfcff; }
pre { background: #f5f5f7; padding: 1rem; overflow: auto; }
.badge { display: inline-block; padding: 0.1rem 0.5rem; border-radius: 999px; font-size: 0.8rem; background: #e8eef7; color: #2a3f5f; }
.muted { color: #777; }
.empty { padding: 2rem; text-align: center; color: #777; background: #fff; border: 1px dashed #ddd; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 1rem; }
.card { background: #fff; padding: 1rem; border-radius: 8px; box-shadow: 0 1px 2px rgba(0,0,0,0.05); }
.card h2 { margin: 0 0 0.25rem; font-size: 1.1rem; }
.card .n { font-size: 2rem; font-weight: 600; color: #2a3f5f; }
a { color: #2a3f5f; }
footer { max-width: 960px; margin: 3rem auto 1.5rem; padding: 1rem 1.25rem; border-top: 1px solid #e0e0e0; color: #777; font-size: 0.85rem; }
footer a { color: #777; }
"#;

fn page(state: &AppState, title: &str, body: &str) -> String {
    let home = state.url("/");
    let mut nav = format!("<a href=\"{}\">mailsift</a>\n", esc(&home));
    for (label, href, present) in [
        ("events", "/events", state.events_dir().is_some()),
        ("bills", "/bills", state.bills_dir().is_some()),
        ("parcels", "/parcels", state.parcels_dir().is_some()),
        ("receipts", "/receipts", state.receipts_dir().is_some()),
        (
            "subscriptions",
            "/subscriptions",
            state.subscriptions_dir().is_some(),
        ),
        ("tickets", "/tickets", state.tickets_dir().is_some()),
    ] {
        if present {
            nav.push_str(&format!(
                "<a href=\"{}\">{}</a>\n",
                esc(&state.url(href)),
                esc(label),
            ));
        }
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>{title} - mailsift</title>\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <style>{CSS}</style>\n</head>\n<body>\n\
         <header>\n{nav}</header>\n\
         <main>\n<h1>{title}</h1>\n{body}\n</main>\n\
         <footer>\n\
         <a href=\"https://github.com/jelmer/mailsift\">mailsift</a> \
         &copy; 2025-2026 Jelmer Vernoo&#307;j \
         &lt;<a href=\"mailto:jelmer@jelmer.uk\">jelmer@jelmer.uk</a>&gt;\n\
         </footer>\n\
         </body>\n</html>",
        title = esc(title),
    )
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Cap on the number of upcoming/recent items on the homepage. Above
/// this, the "view all" link takes over.
const FEED_HOMEPAGE_LIMIT: usize = 20;

async fn index(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    // Only kinds with a configured local dir are shown. The sinks
    // that file elsewhere (CalDAV for events, WebDAV/forwarder for
    // receipts/tickets) have their own UIs.
    let mut cards = Vec::new();
    for (label, href, count) in [
        (
            "events",
            "/events",
            state
                .events_dir()
                .map(|d| count_flat(d, "ics"))
                .transpose()?,
        ),
        (
            "bills",
            "/bills",
            state
                .bills_dir()
                .map(|d| count_year(d, Some("json")))
                .transpose()?,
        ),
        (
            "parcels",
            "/parcels",
            state
                .parcels_dir()
                .map(|d| count_flat(d, "json"))
                .transpose()?,
        ),
        (
            "receipts",
            "/receipts",
            state
                .receipts_dir()
                .map(|d| count_year(d, Some("json")))
                .transpose()?,
        ),
        (
            "subscriptions",
            "/subscriptions",
            state
                .subscriptions_dir()
                .map(|d| count_flat(d, "json"))
                .transpose()?,
        ),
        (
            "tickets",
            "/tickets",
            state
                .tickets_dir()
                .map(|d| count_year(d, None))
                .transpose()?,
        ),
    ] {
        if let Some(count) = count {
            cards.push(format!(
                "<div class=\"card\"><h2><a href=\"{href}\">{label}</a></h2>\
                 <div class=\"n\">{count}</div></div>",
                href = esc(&state.url(href)),
                label = esc(label),
            ));
        }
    }

    let cards_html = format!("<div class=\"grid\">{}</div>", cards.join(""));

    let feed = build_feed(&state)?;
    let (upcoming, recent) = split_feed(&feed);

    let mut body = cards_html;
    body.push_str(&render_feed_sections(
        &state,
        &upcoming,
        &recent,
        FEED_HOMEPAGE_LIMIT,
    ));
    if feed.len() > FEED_HOMEPAGE_LIMIT {
        body.push_str(&format!(
            "<p><a href=\"{}\">View all {} items</a></p>",
            esc(&state.url("/all")),
            feed.len(),
        ));
    }

    Ok(Html(page(&state, "Overview", &body)))
}

/// Partition and sort a feed into (upcoming, recent). Upcoming is
/// soonest first; recent is newest first.
fn split_feed(feed: &[FeedItem]) -> (Vec<&FeedItem>, Vec<&FeedItem>) {
    let today = Utc::now().date_naive();
    let mut upcoming: Vec<&FeedItem> = feed.iter().filter(|i| i.date >= today).collect();
    let mut recent: Vec<&FeedItem> = feed.iter().filter(|i| i.date < today).collect();
    upcoming.sort_by_key(|i| i.date);
    recent.sort_by_key(|i| std::cmp::Reverse(i.date));
    (upcoming, recent)
}

/// Render both sections in order, up to `limit` items each. Empty
/// sections are skipped.
fn render_feed_sections(
    state: &AppState,
    upcoming: &[&FeedItem],
    recent: &[&FeedItem],
    limit: usize,
) -> String {
    let mut out = String::new();
    if !upcoming.is_empty() {
        out.push_str(&render_feed_section(state, "Upcoming", upcoming, limit));
    }
    if !recent.is_empty() {
        out.push_str(&render_feed_section(state, "Recent", recent, limit));
    }
    out
}

fn render_feed_section(state: &AppState, title: &str, items: &[&FeedItem], limit: usize) -> String {
    let mut rows = String::new();
    for item in items.iter().take(limit) {
        rows.push_str(&format!(
            "<tr><td>{date}</td><td><span class=\"badge\">{kind}</span></td>\
             <td>{title}</td><td class=\"muted\">{subtitle}</td>\
             <td><a href=\"{href}\">open</a></td></tr>",
            date = esc(&item.date.to_string()),
            kind = esc(item.kind),
            title = esc(&item.title),
            subtitle = esc(&item.subtitle),
            href = esc(&state.url(&item.href)),
        ));
    }
    format!(
        "<h2>{title}</h2>\
         <table><thead><tr><th>date</th><th>type</th><th></th><th></th><th></th></tr></thead>\
         <tbody>{rows}</tbody></table>",
        title = esc(title),
    )
}

async fn list_all(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let feed = build_feed(&state)?;
    let (upcoming, recent) = split_feed(&feed);
    let body = if feed.is_empty() {
        "<div class=\"empty\">no artifacts yet</div>".to_string()
    } else {
        render_feed_sections(&state, &upcoming, &recent, usize::MAX)
    };
    Ok(Html(page(&state, "All artifacts", &body)))
}

/// A single row in the merged homepage feed. Every artifact type
/// contributes zero or more of these, tagged with an [`AppState`]-
/// relative URL and a date used for sorting.
#[derive(Debug, Clone)]
struct FeedItem {
    /// Sort key. Extracted from a per-kind field (dueDate, orderDate,
    /// DTSTART, ...) when available; falls back to file mtime.
    date: NaiveDate,
    /// Kind label rendered as a badge in the list.
    kind: &'static str,
    /// Free-form title (payee, merchant, event summary, ...).
    title: String,
    /// Secondary line (invoice number, order number, tracking, ...).
    subtitle: String,
    /// Root-relative URL to the detail page; run through `state.url`
    /// before rendering.
    href: String,
}

/// Try a series of ISO-ish date formats. Accepts YYYY-MM-DD,
/// YYYY-MM-DDTHH:MM:SS(Z|+00:00), and iCal `YYYYMMDDTHHMMSSZ`.
fn parse_any_date(raw: &str) -> Option<NaiveDate> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    // strip fractional seconds + timezone before parsing.
    let head = s.split('.').next().unwrap_or(s);
    let head = head
        .strip_suffix('Z')
        .or_else(|| head.split(['+', '-']).next())
        .unwrap_or(head);
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y%m%dT%H%M%S", "%Y%m%d"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(head, fmt) {
            return Some(dt.date());
        }
        if fmt == "%Y%m%d"
            && let Ok(d) = NaiveDate::parse_from_str(head, fmt)
        {
            return Some(d);
        }
    }
    None
}

/// Gather every artifact into a single date-sorted feed. Missing dates
/// fall back to the file mtime, so nothing is silently dropped.
fn build_feed(state: &AppState) -> Result<Vec<FeedItem>> {
    let mut items: Vec<FeedItem> = Vec::new();

    if let Some(dir) = state.events_dir() {
        for entry in read_dir_or_empty(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file()
                || path
                    .extension()
                    .is_none_or(|e| !e.eq_ignore_ascii_case("ics"))
            {
                continue;
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let body = fs::read_to_string(&path).unwrap_or_default();
            let summary = ics_field(&body, "SUMMARY").unwrap_or_else(|| stem.clone());
            let dtstart = ics_field(&body, "DTSTART").and_then(|d| parse_any_date(&d));
            let date = dtstart.unwrap_or_else(|| mtime_date(&path));
            items.push(FeedItem {
                date,
                kind: "event",
                title: summary,
                subtitle: stem,
                href: format!("/events/{name}"),
            });
        }
    }

    if let Some(dir) = state.bills_dir() {
        for (year, slug, value) in walk_year_json(dir)? {
            let payee = pick_str(&value, &["payee", "accountName"]).unwrap_or_default();
            let invoice = pick_str(&value, &["invoiceNumber", "identifier"]).unwrap_or_default();
            let date = pick_str(&value, &["dueDate", "paymentDueDate", "date", "issueDate"])
                .and_then(|d| parse_any_date(&d))
                .unwrap_or_else(|| mtime_date(&dir.join(&year).join(format!("{slug}.json"))));
            items.push(FeedItem {
                date,
                kind: "bill",
                title: payee,
                subtitle: invoice,
                href: format!("/bills/{year}/{slug}.json"),
            });
        }
    }

    if let Some(dir) = state.parcels_dir() {
        for (name, value) in walk_flat_json(dir)? {
            let tracking = pick_str(&value, &["trackingNumber", "identifier"]).unwrap_or_default();
            let status = pick_str(&value, &["deliveryStatus"]).unwrap_or_default();
            let date = pick_str(
                &value,
                &[
                    "actualDeliveryTime",
                    "expectedArrivalUntil",
                    "expectedArrivalFrom",
                ],
            )
            .and_then(|d| parse_any_date(&d))
            .unwrap_or_else(|| mtime_date(&dir.join(&name)));
            items.push(FeedItem {
                date,
                kind: "parcel",
                title: tracking,
                subtitle: status,
                href: format!("/parcels/{name}"),
            });
        }
    }

    if let Some(dir) = state.receipts_dir() {
        for (year, slug, value) in walk_year_json(dir)? {
            let merchant = pick_str(&value, &["merchant", "seller"]).unwrap_or_default();
            let order = pick_str(&value, &["orderNumber", "identifier"]).unwrap_or_default();
            let date = pick_str(&value, &["orderDate", "date"])
                .and_then(|d| parse_any_date(&d))
                .unwrap_or_else(|| mtime_date(&dir.join(&year).join(format!("{slug}.json"))));
            items.push(FeedItem {
                date,
                kind: "receipt",
                title: merchant,
                subtitle: order,
                href: format!("/receipts/{year}/{slug}.json"),
            });
        }
    }

    if let Some(dir) = state.subscriptions_dir() {
        for (name, value) in walk_flat_json(dir)? {
            let display = pick_str(&value, &["name", "provider"]).unwrap_or_default();
            let renewal = pick_str(&value, &["renewalDate", "nextPaymentDate"]);
            let date = renewal
                .as_deref()
                .and_then(parse_any_date)
                .unwrap_or_else(|| mtime_date(&dir.join(&name)));
            items.push(FeedItem {
                date,
                kind: "subscription",
                title: display,
                subtitle: renewal.unwrap_or_default(),
                href: format!("/subscriptions/{name}"),
            });
        }
    }

    if let Some(dir) = state.tickets_dir() {
        for entry in read_dir_or_empty(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let year = entry.file_name().to_string_lossy().into_owned();
            let year_num: i32 = year.parse().unwrap_or(0);
            for inner in fs::read_dir(entry.path())? {
                let inner = inner?;
                if !inner.file_type()?.is_file() {
                    continue;
                }
                let name = inner.file_name().to_string_lossy().into_owned();
                // Tickets don't carry their own date; use file mtime,
                // falling back to Jan 1 of the year dir if we can't
                // stat.
                let date = mtime_date_opt(&inner.path())
                    .or_else(|| NaiveDate::from_ymd_opt(year_num, 1, 1))
                    .unwrap_or_else(|| Utc::now().date_naive());
                items.push(FeedItem {
                    date,
                    kind: "ticket",
                    title: name.clone(),
                    subtitle: year.clone(),
                    href: format!("/tickets/{year}/{name}"),
                });
            }
        }
    }

    Ok(items)
}

/// File mtime as a naive UTC date. On any error (missing file, no
/// mtime, out-of-range) fall back to today so the item still surfaces
/// in the feed.
fn mtime_date(path: &Path) -> NaiveDate {
    mtime_date_opt(path).unwrap_or_else(|| Utc::now().date_naive())
}

fn mtime_date_opt(path: &Path) -> Option<NaiveDate> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let dt = chrono::DateTime::from_timestamp(secs, 0)?;
    Some(dt.date_naive())
}

/// Count top-level files under `dir` whose extension matches `ext`
/// (case-insensitive). Used for the flat kinds (parcels, subscriptions,
/// events).
fn count_flat(dir: &Path, ext: &str) -> Result<usize> {
    let mut n = 0;
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            n += 1;
        }
    }
    Ok(n)
}

/// Count files two levels down under `dir` (i.e. `<year>/<file>`). If
/// `ext` is `Some`, only files with that extension count; otherwise
/// every file counts.
fn count_year(dir: &Path, ext: Option<&str>) -> Result<usize> {
    let mut n = 0;
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        for inner in fs::read_dir(entry.path())? {
            let inner = inner?;
            if !inner.file_type()?.is_file() {
                continue;
            }
            if let Some(want) = ext
                && !inner
                    .path()
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case(want))
            {
                continue;
            }
            n += 1;
        }
    }
    Ok(n)
}

/// Read a directory, or an empty iterator if it doesn't exist yet.
///
/// The artifact dirs are created lazily by the pipeline on first
/// filing, so a fresh install has none of them and we shouldn't 500.
fn read_dir_or_empty(dir: &Path) -> Result<Box<dyn Iterator<Item = io::Result<fs::DirEntry>>>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(Box::new(rd)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Box::new(std::iter::empty())),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", dir.display()))),
    }
}

fn require_dir<'a>(dir: Option<&'a Path>, label: &str) -> Result<&'a Path, AppError> {
    dir.ok_or_else(|| AppError {
        status: StatusCode::NOT_FOUND,
        err: anyhow::anyhow!("no {label} directory configured; set {label}_dir in your config"),
    })
}

/// Reject `.`, `..`, absolute paths, and anything that would traverse
/// outside the artifact dir. Applied to every path segment coming off
/// the URL before we join it onto a filesystem path.
fn safe_segment(seg: &str) -> Result<&str, AppError> {
    if seg.is_empty()
        || seg == "."
        || seg == ".."
        || seg.contains('/')
        || seg.contains('\\')
        || seg.contains('\0')
    {
        return Err(AppError::bad_request("invalid path segment"));
    }
    // Belt-and-braces: even though the above catches slashes, run the
    // segment through PathBuf::components() to make sure nothing weird
    // survives (e.g. platform-specific traversal).
    let pb = PathBuf::from(seg);
    if pb.components().count() != 1 || !matches!(pb.components().next(), Some(Component::Normal(_)))
    {
        return Err(AppError::bad_request("invalid path segment"));
    }
    Ok(seg)
}

async fn list_events(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.events_dir(), "events")?;
    let mut rows: Vec<String> = Vec::new();
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        if path
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("ics"))
        {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let body = fs::read_to_string(&path).ok().unwrap_or_default();
        let summary = ics_field(&body, "SUMMARY").unwrap_or_else(|| stem.clone());
        let dtstart = ics_field(&body, "DTSTART").unwrap_or_default();
        rows.push(format!(
            "<tr><td>{}</td><td>{}</td><td><a href=\"{}\">{}</a></td></tr>",
            esc(&dtstart),
            esc(&summary),
            esc(&state.url(&format!("/events/{name}"))),
            esc(&stem),
        ));
    }
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Events",
            "<div class=\"empty\">no events</div>",
        )));
    }
    let body = format!(
        "<table><thead><tr><th>starts</th><th>summary</th><th>UID</th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.join("")
    );
    Ok(Html(page(&state, "Events", &body)))
}

async fn get_event(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.events_dir(), "events")?;
    let name = safe_segment(&name)?;
    let path = dir.join(name);
    let body = fs::read(&path).map_err(|e| read_status(&path, e))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/calendar; charset=utf-8"),
        )],
        body,
    )
        .into_response())
}

/// Unfold and pick the value of the first line whose name (before any
/// `;PARAM=` or `:`) matches `key`. RFC 5545 lines can be folded across
/// multiple physical lines with a leading space or tab; iCalendar
/// consumers unfold before parsing.
fn ics_field(body: &str, key: &str) -> Option<String> {
    let mut logical = String::new();
    let mut lines: Vec<String> = Vec::new();
    for raw in body.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            logical.push_str(&raw[1..]);
        } else {
            if !logical.is_empty() {
                lines.push(std::mem::take(&mut logical));
            }
            logical.push_str(raw);
        }
    }
    if !logical.is_empty() {
        lines.push(logical);
    }
    for line in lines {
        let sep = line.find([':', ';']).unwrap_or(line.len());
        let name = &line[..sep];
        if name.eq_ignore_ascii_case(key) {
            let colon = line.find(':')?;
            return Some(line[colon + 1..].to_string());
        }
    }
    None
}

async fn list_bills(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.bills_dir(), "bills")?;
    let mut rows: Vec<(String, String, String)> = Vec::new(); // (year, slug, cells)
    for (year, slug, value) in walk_year_json(dir)? {
        let payee = pick_str(&value, &["payee", "accountName"]);
        let invoice = pick_str(&value, &["invoiceNumber", "identifier"]);
        let due = pick_str(&value, &["dueDate", "paymentDueDate", "date"]);
        let amount = value
            .get("totalPaymentDue")
            .and_then(|p| {
                let price = p.get("price")?;
                let cur = p
                    .get("priceCurrency")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                Some(format!("{} {}", price, cur).trim().to_string())
            })
            .unwrap_or_default();
        let href = state.url(&format!("/bills/{}/{}.json", year, slug));
        let cells = format!(
            "<td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"{}\">json</a></td>",
            esc(&year),
            esc(&payee.unwrap_or_default()),
            esc(&invoice.unwrap_or_default()),
            esc(&due.unwrap_or_default()),
            esc(&amount),
            esc(&href),
        );
        rows.push((year, slug, cells));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Bills",
            "<div class=\"empty\">no bills</div>",
        )));
    }
    let body = format!(
        "<table><thead><tr><th>year</th><th>payee</th><th>invoice</th><th>due</th><th>amount</th><th></th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.into_iter()
            .map(|(_, _, cells)| format!("<tr>{cells}</tr>"))
            .collect::<Vec<_>>()
            .join("")
    );
    Ok(Html(page(&state, "Bills", &body)))
}

async fn get_bill(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.bills_dir(), "bills")?;
    serve_json_file(dir, &year, &name)
}

async fn list_parcels(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.parcels_dir(), "parcels")?;
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, value) in walk_flat_json(dir)? {
        let tracking = pick_str(&value, &["trackingNumber", "identifier"]).unwrap_or_default();
        let status = pick_str(&value, &["deliveryStatus"]).unwrap_or_default();
        let carrier = value
            .get("provider")
            .and_then(|p| p.get("@id").or_else(|| p.get("name")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let eta =
            pick_str(&value, &["expectedArrivalUntil", "actualDeliveryTime"]).unwrap_or_default();
        let cells = format!(
            "<td>{}</td><td><span class=\"badge\">{}</span></td><td>{}</td><td>{}</td>\
             <td><a href=\"{}\">json</a></td>",
            esc(&tracking),
            esc(&carrier),
            esc(&status),
            esc(&eta),
            esc(&state.url(&format!("/parcels/{name}"))),
        );
        rows.push((name, cells));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Parcels",
            "<div class=\"empty\">no parcels</div>",
        )));
    }
    let body = format!(
        "<table><thead><tr><th>tracking</th><th>carrier</th><th>status</th><th>eta</th><th></th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.into_iter()
            .map(|(_, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join("")
    );
    Ok(Html(page(&state, "Parcels", &body)))
}

async fn get_parcel(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.parcels_dir(), "parcels")?;
    let name = safe_segment(&name)?;
    let path = dir.join(name);
    let body = fs::read(&path).map_err(|e| read_status(&path, e))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response())
}

async fn list_receipts(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for (year, slug, value) in walk_year_json(dir)? {
        let merchant = pick_str(&value, &["merchant", "seller"]).unwrap_or_default();
        let order = pick_str(&value, &["orderNumber", "identifier"]).unwrap_or_default();
        let date = pick_str(&value, &["orderDate", "date"]).unwrap_or_default();
        let cells = format!(
            "<td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
             <td><a href=\"{}\">json</a></td>",
            esc(&year),
            esc(&merchant),
            esc(&order),
            esc(&date),
            esc(&state.url(&format!("/receipts/{year}/{slug}.json"))),
        );
        rows.push((year, slug, cells));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Receipts",
            "<div class=\"empty\">no receipts</div>",
        )));
    }
    let body = format!(
        "<table><thead><tr><th>year</th><th>merchant</th><th>order</th><th>date</th><th></th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.into_iter()
            .map(|(_, _, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join("")
    );
    Ok(Html(page(&state, "Receipts", &body)))
}

async fn get_receipt(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    serve_json_file(dir, &year, &name)
}

async fn list_subscriptions(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.subscriptions_dir(), "subscriptions")?;
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, value) in walk_flat_json(dir)? {
        let display = pick_str(&value, &["name", "provider"]).unwrap_or_default();
        let renewal = pick_str(&value, &["renewalDate", "nextPaymentDate"]).unwrap_or_default();
        let price = value
            .get("price")
            .and_then(|v| v.as_str().map(String::from).or_else(|| Some(v.to_string())))
            .unwrap_or_default();
        let cells = format!(
            "<td>{}</td><td>{}</td><td>{}</td><td><a href=\"{}\">json</a></td>",
            esc(&display),
            esc(&renewal),
            esc(&price),
            esc(&state.url(&format!("/subscriptions/{name}"))),
        );
        rows.push((name, cells));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Subscriptions",
            "<div class=\"empty\">no subscriptions</div>",
        )));
    }
    let body = format!(
        "<table><thead><tr><th>name</th><th>renews</th><th>price</th><th></th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.into_iter()
            .map(|(_, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join("")
    );
    Ok(Html(page(&state, "Subscriptions", &body)))
}

async fn get_subscription(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.subscriptions_dir(), "subscriptions")?;
    let name = safe_segment(&name)?;
    let path = dir.join(name);
    let body = fs::read(&path).map_err(|e| read_status(&path, e))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response())
}

async fn list_tickets(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.tickets_dir(), "tickets")?;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let year = entry.file_name().to_string_lossy().into_owned();
        for inner in fs::read_dir(entry.path())? {
            let inner = inner?;
            if !inner.file_type()?.is_file() {
                continue;
            }
            let name = inner.file_name().to_string_lossy().into_owned();
            let size = inner.metadata().map(|m| m.len()).unwrap_or(0);
            let ext = inner
                .path()
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            let cells = format!(
                "<td>{}</td><td><span class=\"badge\">{}</span></td><td>{}</td><td>{}</td>\
                 <td><a href=\"{}\">download</a></td>",
                esc(&year),
                esc(&ext),
                esc(&name),
                human_size(size),
                esc(&state.url(&format!("/tickets/{year}/{name}"))),
            );
            rows.push((year.clone(), name, cells));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Tickets",
            "<div class=\"empty\">no tickets</div>",
        )));
    }
    let body = format!(
        "<table><thead><tr><th>year</th><th>type</th><th>name</th><th>size</th><th></th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.into_iter()
            .map(|(_, _, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join("")
    );
    Ok(Html(page(&state, "Tickets", &body)))
}

async fn get_ticket(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.tickets_dir(), "tickets")?;
    let year = safe_segment(&year)?;
    let name = safe_segment(&name)?;
    let path = dir.join(year).join(name);
    let body = fs::read(&path).map_err(|e| read_status(&path, e))?;
    let ct = content_type_for(name);
    Ok(([(header::CONTENT_TYPE, HeaderValue::from_static(ct))], body).into_response())
}

fn content_type_for(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "pdf" => "application/pdf",
        "pkpass" => "application/vnd.apple.pkpass",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

async fn api_bills(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let dir = require_dir(state.bills_dir(), "bills")?;
    let items: Vec<Value> = walk_year_json(dir)?
        .into_iter()
        .map(|(year, slug, mut v)| {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("_year".into(), Value::String(year));
                obj.insert("_slug".into(), Value::String(slug));
            }
            v
        })
        .collect();
    Ok(Json(Value::Array(items)))
}

async fn api_parcels(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let dir = require_dir(state.parcels_dir(), "parcels")?;
    let items: Vec<Value> = walk_flat_json(dir)?.into_iter().map(|(_, v)| v).collect();
    Ok(Json(Value::Array(items)))
}

async fn api_receipts(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    let items: Vec<Value> = walk_year_json(dir)?
        .into_iter()
        .map(|(year, slug, mut v)| {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("_year".into(), Value::String(year));
                obj.insert("_slug".into(), Value::String(slug));
            }
            v
        })
        .collect();
    Ok(Json(Value::Array(items)))
}

async fn api_subscriptions(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let dir = require_dir(state.subscriptions_dir(), "subscriptions")?;
    let items: Vec<Value> = walk_flat_json(dir)?.into_iter().map(|(_, v)| v).collect();
    Ok(Json(Value::Array(items)))
}

/// (filename, parsed JSON) for every `*.json` directly under `dir`.
fn walk_flat_json(dir: &Path) -> Result<Vec<(String, Value)>> {
    let mut out = Vec::new();
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let value = read_json(&path)?;
        out.push((name, value));
    }
    Ok(out)
}

/// (year, `<stem>` without `.json`, parsed JSON) for every
/// `<year>/<stem>.json` two levels down under `dir`.
fn walk_year_json(dir: &Path) -> Result<Vec<(String, String, Value)>> {
    let mut out = Vec::new();
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let year = entry.file_name().to_string_lossy().into_owned();
        for inner in fs::read_dir(entry.path())? {
            let inner = inner?;
            if !inner.file_type()?.is_file() {
                continue;
            }
            let path = inner.path();
            if path
                .extension()
                .is_none_or(|e| !e.eq_ignore_ascii_case("json"))
            {
                continue;
            }
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let value = read_json(&path)?;
            out.push((year.clone(), stem, value));
        }
    }
    Ok(out)
}

fn read_json(path: &Path) -> Result<Value> {
    let body = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))
}

fn serve_json_file(dir: &Path, year: &str, name: &str) -> Result<Response, AppError> {
    let year = safe_segment(year)?;
    let name = safe_segment(name)?;
    let path = dir.join(year).join(name);
    let body = fs::read(&path).map_err(|e| read_status(&path, e))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response())
}

/// Try a series of keys and return the first non-empty string value.
fn pick_str(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(v) = value.get(*key)
            && let Some(s) = v.as_str()
            && !s.trim().is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
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

    #[test]
    fn parse_any_date_iso_and_ical() {
        assert_eq!(
            parse_any_date("2026-05-01"),
            NaiveDate::from_ymd_opt(2026, 5, 1)
        );
        assert_eq!(
            parse_any_date("2026-08-27T18:00:00Z"),
            NaiveDate::from_ymd_opt(2026, 8, 27)
        );
        assert_eq!(
            parse_any_date("20260201T100000Z"),
            NaiveDate::from_ymd_opt(2026, 2, 1)
        );
        assert_eq!(
            parse_any_date("20260201"),
            NaiveDate::from_ymd_opt(2026, 2, 1)
        );
        assert!(parse_any_date("not a date").is_none());
        assert!(parse_any_date("").is_none());
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

    #[test]
    fn ics_field_unfolds() {
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Flight\r\n LHR to CDG\r\n\
                    DTSTART:20260201T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            ics_field(body, "SUMMARY"),
            Some("FlightLHR to CDG".to_string())
        );
        assert_eq!(
            ics_field(body, "DTSTART"),
            Some("20260201T100000Z".to_string())
        );
    }

    #[test]
    fn ics_field_ignores_params() {
        let body = "DTSTART;TZID=Europe/London:20260201T100000\r\n";
        assert_eq!(
            ics_field(body, "DTSTART"),
            Some("20260201T100000".to_string())
        );
    }

    #[test]
    fn safe_segment_rejects_traversal() {
        assert!(safe_segment("..").is_err());
        assert!(safe_segment("a/b").is_err());
        assert!(safe_segment("").is_err());
        assert!(safe_segment("ok.json").is_ok());
    }

    #[test]
    fn walk_year_json_skips_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(walk_year_json(&missing).unwrap().is_empty());
    }

    #[test]
    fn walk_flat_json_reads_top_level() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.json"), br#"{"x":1}"#).unwrap();
        fs::write(tmp.path().join("b.txt"), b"skip").unwrap();
        let items = walk_flat_json(tmp.path()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "a.json");
    }

    #[test]
    fn walk_year_json_reads_year_slug() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("2026")).unwrap();
        fs::write(
            tmp.path().join("2026/vendor-INV1.json"),
            br#"{"payee":"vendor","invoiceNumber":"INV1"}"#,
        )
        .unwrap();
        let items = walk_year_json(tmp.path()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "2026");
        assert_eq!(items[0].1, "vendor-INV1");
    }

    #[test]
    fn pick_str_returns_first_non_empty() {
        let v: Value = serde_json::from_str(r#"{"a":"","b":"hit","c":"skip"}"#).unwrap();
        assert_eq!(pick_str(&v, &["a", "b", "c"]), Some("hit".into()));
    }

    #[test]
    fn human_size_formats() {
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(2 * 1024 * 1024), "2.0 MB");
    }
}
