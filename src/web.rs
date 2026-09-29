//! Read-only HTTP dashboard for the artifact directories.
//!
//! Scans the configured `bills_dir`, `parcels_dir`, `receipts_dir`,
//! `subscriptions_dir`, `events_dir`, `reservations_dir`, and
//! `tickets_dir` on each request
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
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
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

    /// Archived reservation JSON. Independent of `events_dir`: a
    /// reservation is always turned into a calendar event, and the
    /// full record is additionally kept here when configured.
    fn reservations_dir(&self) -> Option<&Path> {
        self.config.reservations_dir.as_deref()
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
        .route("/bills/:year/:slug/view", get(view_bill))
        .route("/bills/:year/:name", get(get_bill))
        .route("/parcels", get(list_parcels))
        .route("/parcels/:name/view", get(view_parcel))
        .route("/parcels/:name", get(get_parcel))
        .route("/receipts", get(list_receipts))
        .route("/receipts/:year/:slug/view", get(view_receipt))
        .route("/receipts/:year/:name", get(get_receipt))
        .route("/subscriptions", get(list_subscriptions))
        .route("/subscriptions/:name/view", get(view_subscription))
        .route("/subscriptions/:name", get(get_subscription))
        .route("/reservations", get(list_reservations))
        .route("/reservations/:year/:slug/view", get(view_reservation))
        .route("/reservations/:year/:name", get(get_reservation))
        .route("/tickets", get(list_tickets))
        .route("/tickets/:year/:slug/view", get(view_ticket))
        .route("/tickets/:year/:name", get(get_ticket))
        .route("/stats", get(show_stats))
        .route("/api/bills.json", get(api_bills))
        .route("/api/parcels.json", get(api_parcels))
        .route("/api/receipts.json", get(api_receipts))
        .route("/api/subscriptions.json", get(api_subscriptions))
        .route("/api/reservations.json", get(api_reservations))
        .route("/api/stats.json", get(api_stats))
        .route("/api/recent-failures.json", get(api_recent_failures))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rerender_error_pages,
        ))
        .with_state(state)
}

/// Fallback for unrouted paths. Uses the request's state so the 404
/// page keeps the full navbar (`AppError::into_response` can't since
/// axum's `IntoResponse` isn't given state).
async fn not_found(State(state): State<Arc<AppState>>) -> Response {
    error_page(&state, StatusCode::NOT_FOUND)
}

/// Middleware that rewrites HTML error responses to include the full
/// navbar. `AppError::into_response` renders with an empty
/// `AppState::default()`; here we swap that body for one rendered with
/// the real state. Non-HTML responses (JSON APIs, PDF blobs) pass
/// through untouched.
async fn rerender_error_pages(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let resp = next.run(req).await;
    if !resp.status().is_client_error() && !resp.status().is_server_error() {
        return resp;
    }
    let is_html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if !is_html {
        return resp;
    }
    error_page(&state, resp.status())
}

/// Render an error page with the request's [`AppState`] so the navbar
/// is populated. Used by the router `fallback` and the
/// `rerender_error_pages` middleware.
fn error_page(state: &AppState, status: StatusCode) -> Response {
    let reason = status.canonical_reason().unwrap_or("Error");
    (
        status,
        Html(page(state, reason, &format!("<p>{}</p>", esc(reason)))),
    )
        .into_response()
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
th.num, td.num { text-align: right; font-variant-numeric: tabular-nums; }
tr:hover td { background: #fbfcff; }
pre { background: #f5f5f7; padding: 1rem; overflow: auto; }
.badge { display: inline-block; padding: 0.1rem 0.5rem; border-radius: 999px; font-size: 0.8rem; background: #e8eef7; color: #2a3f5f; }
.badge.bill { background: #fbe6d4; color: #7a3d00; }
.badge.parcel { background: #d9ebd2; color: #274d1f; }
.badge.receipt { background: #e2d8f4; color: #422d76; }
.badge.subscription { background: #f7dee6; color: #7d1f3d; }
.badge.reservation { background: #d3e8f7; color: #14416b; }
.badge.ticket { background: #f5efc9; color: #6b5510; }
.badge.event { background: #d8e6e2; color: #234942; }
.badge.muted-badge { background: #ececec; color: #666; }
.muted { color: #777; }
.pager { margin: 1rem 0; }
.pager a, .pager span { margin-right: 0.5rem; }
.filter-bar { display: flex; flex-wrap: wrap; gap: 1rem; align-items: center; margin: 0 0 1rem; }
.filter-bar form.search { display: flex; gap: 0.4rem; }
.filter-bar input[type=search] { padding: 0.35rem 0.6rem; border: 1px solid #ccc; border-radius: 4px; font: inherit; }
.filter-bar button { padding: 0.35rem 0.75rem; border: 1px solid #2a3f5f; background: #2a3f5f; color: #fff; border-radius: 4px; cursor: pointer; font: inherit; }
.filter-bar button:hover { background: #1e2e46; }
.year-chips { display: flex; flex-wrap: wrap; gap: 0.35rem; }
.year-chips a.chip { display: inline-block; padding: 0.15rem 0.55rem; border-radius: 999px; background: #eef1f6; color: #2a3f5f; font-size: 0.85rem; text-decoration: none; }
.year-chips a.chip:hover { background: #dde3ed; }
.year-chips a.chip.active { background: #2a3f5f; color: #fff; }
dl.detail { display: grid; grid-template-columns: max-content 1fr; column-gap: 1rem; row-gap: 0.35rem; background: #fff; padding: 1rem; border-radius: 6px; box-shadow: 0 1px 2px rgba(0,0,0,0.05); margin: 0 0 1rem; }
dl.detail dt { color: #555; font-weight: 500; }
dl.detail dd { margin: 0; }
p.links { margin: 0.5rem 0; }
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
        (
            "reservations",
            "/reservations",
            state.reservations_dir().is_some(),
        ),
        ("tickets", "/tickets", state.tickets_dir().is_some()),
        // Stats is always in the nav: if there's no log yet the
        // page renders an "empty" panel telling you where mailsift
        // would write one.
        ("stats", "/stats", true),
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
    let subscription_totals = state
        .subscriptions_dir()
        .map(count_active_subscriptions)
        .transpose()?;
    for (label, href, count, subtitle) in [
        (
            "events",
            "/events",
            state
                .events_dir()
                .map(|d| count_flat(d, "ics"))
                .transpose()?,
            None,
        ),
        (
            "bills",
            "/bills",
            state
                .bills_dir()
                .map(|d| count_year(d, Some("json")))
                .transpose()?,
            None,
        ),
        (
            "parcels",
            "/parcels",
            state
                .parcels_dir()
                .map(|d| count_flat(d, "json"))
                .transpose()?,
            None,
        ),
        (
            "receipts",
            "/receipts",
            state
                .receipts_dir()
                .map(|d| count_year(d, Some("json")))
                .transpose()?,
            None,
        ),
        (
            "subscriptions",
            "/subscriptions",
            subscription_totals.map(|(_, total)| total),
            subscription_totals.map(|(active, _)| format!("{active} active")),
        ),
        (
            "reservations",
            "/reservations",
            state
                .reservations_dir()
                .map(|d| count_year(d, Some("json")))
                .transpose()?,
            None,
        ),
        (
            "tickets",
            "/tickets",
            state.tickets_dir().map(count_ticket_groups).transpose()?,
            None,
        ),
    ] {
        if let Some(count) = count {
            let subtitle_html = subtitle
                .as_deref()
                .map(|s| format!("<div class=\"muted\">{}</div>", esc(s)))
                .unwrap_or_default();
            cards.push(format!(
                "<div class=\"card\"><h2><a href=\"{href}\">{label}</a></h2>\
                 <div class=\"n\">{count}</div>{subtitle_html}</div>",
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
            "<tr><td>{date}</td><td><span class=\"badge {kind_class}\">{kind}</span></td>\
             <td>{title}</td><td class=\"muted\">{subtitle}</td>\
             <td>{links}</td></tr>",
            date = esc(&item.date.to_string()),
            kind_class = esc(item.kind),
            kind = esc(item.kind),
            title = esc(&item.title),
            subtitle = esc(&item.subtitle),
            links = links_cell(
                &state.url(&item.href),
                &item
                    .blobs
                    .iter()
                    .map(|(label, path)| (label.clone(), state.url(path)))
                    .collect::<Vec<_>>(),
                item.vendor_url.as_deref(),
            ),
        ));
    }
    format!(
        "<h2>{title}</h2>\
         <table><thead><tr><th>date</th><th>type</th><th></th><th></th><th>links</th></tr></thead>\
         <tbody>{rows}</tbody></table>",
        title = esc(title),
    )
}

async fn list_all(
    State(state): State<Arc<AppState>>,
    Query(page_q): Query<PageQuery>,
) -> Result<Html<String>, AppError> {
    let feed = build_feed(&state)?;
    let (upcoming, recent) = split_feed(&feed);
    let body = if feed.is_empty() {
        "<div class=\"empty\">no artifacts yet</div>".to_string()
    } else {
        // Upcoming stays fully rendered (typically small); Recent
        // dominates the row count once you have any history, so that's
        // the section we paginate.
        let mut out = String::new();
        if !upcoming.is_empty() {
            out.push_str(&render_feed_section(
                &state,
                "Upcoming",
                &upcoming,
                usize::MAX,
            ));
        }
        if !recent.is_empty() {
            let (page_slice, pager) = paginate(&recent, &page_q, &state.url("/all"));
            out.push_str(&render_feed_section(
                &state,
                "Recent",
                page_slice,
                usize::MAX,
            ));
            out.push_str(&pager.render(recent.len()));
        }
        out
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
    /// Vendor / detail URL from the artifact JSON, if present.
    vendor_url: Option<String>,
    /// Root-relative URL to the detail page; run through `state.url`
    /// before rendering.
    href: String,
    /// Companion downloadable blobs (e.g. `("pdf",
    /// "/bills/2026/acme.pdf")`) grouped with this entry. Rendered as
    /// extra links after the primary JSON link.
    blobs: Vec<(String, String)>,
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
    // Strip fractional seconds + timezone before parsing. The offset
    // is only looked for after the `T`: splitting the whole string on
    // `-` would cut an ISO date down to its year.
    let head = s.split('.').next().unwrap_or(s);
    let head = match head.strip_suffix('Z') {
        Some(without_z) => without_z,
        None => match head.find('T') {
            Some(t) => match head[t..].find(['+', '-']) {
                Some(off) => &head[..t + off],
                None => head,
            },
            None => head,
        },
    };
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

/// Render a date-like string as `YYYY-MM-DD` when it parses as one of
/// the ISO variants [`parse_any_date`] accepts. Anything unrecognised
/// passes through unchanged so we never silently blank out a value we
/// don't understand.
fn short_date(raw: &str) -> String {
    parse_any_date(raw)
        .map(|d| d.to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// Query parameters shared by every list page: pagination, a
/// substring search, and a shard-year filter. All fields are optional
/// and lenient — values that don't parse fall back to `None` rather
/// than 400ing a shared URL.
#[derive(Default)]
struct ListQuery {
    page: Option<usize>,
    q: Option<String>,
    year: Option<String>,
}

impl<'de> Deserialize<'de> for ListQuery {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            page: Option<String>,
            q: Option<String>,
            year: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(ListQuery {
            page: raw.page.and_then(|s| s.parse().ok()),
            q: raw.q.filter(|s| !s.is_empty()),
            year: raw.year.filter(|s| !s.is_empty()),
        })
    }
}

/// Backwards-compatible alias so older call sites and tests keep
/// working; the new name reflects the broader remit.
type PageQuery = ListQuery;

impl ListQuery {
    /// Serialise the non-empty fields to a URL query string prefixed
    /// with `?`, or the empty string if nothing is set. Used to build
    /// pager `href`s that preserve the current filters.
    fn to_query_string(&self, override_page: Option<usize>) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(q) = &self.q {
            parts.push(format!("q={}", url_encode(q)));
        }
        if let Some(y) = &self.year {
            parts.push(format!("year={}", url_encode(y)));
        }
        if let Some(p) = override_page.or(self.page)
            && p > 1
        {
            parts.push(format!("page={p}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }
}

/// Minimal percent-encoder for query values: spaces to `+`, everything
/// non-alphanumeric to `%XX`. Good enough for the free-text `q=` and
/// four-digit `year=` values we actually emit.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Rows-per-page for the list views. Big enough to fit the "few
/// hundred" typical size without a scroll of doom, small enough that
/// the HTML for a page stays under ~80 KB even with long URLs in
/// vendor links.
const PAGE_SIZE: usize = 100;

/// Paginate `rows` for `query`, returning the slice for the current
/// page plus a pre-built [`Pager`] describing prev/next links relative
/// to `base_url`. `base_url` should not carry its own query string;
/// filter parameters from `query` are appended when the pager renders.
fn paginate<'a, T>(rows: &'a [T], query: &ListQuery, base_url: &str) -> (&'a [T], Pager) {
    let total = rows.len();
    let pages = total.div_ceil(PAGE_SIZE).max(1);
    let page = query.page.unwrap_or(1).clamp(1, pages);
    let start = (page - 1) * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(total);
    (
        &rows[start..end],
        Pager {
            page,
            pages,
            base_url: base_url.to_string(),
            prev_query: query.to_query_string(Some(page.saturating_sub(1).max(1))),
            next_query: query.to_query_string(Some(page + 1)),
        },
    )
}

/// Pagination metadata computed by [`paginate`]. Kept separate from the
/// row slice so callers can render it in whatever spot they want.
struct Pager {
    page: usize,
    pages: usize,
    base_url: String,
    /// Pre-built query strings for prev/next links so filters (`q=`,
    /// `year=`) survive pagination.
    prev_query: String,
    next_query: String,
}

impl Pager {
    /// Render the prev / page-x-of-y / next line, or an empty string
    /// when there's only one page. `total` is the full row count and
    /// gets shown alongside the page counter.
    fn render(&self, total: usize) -> String {
        if self.pages <= 1 {
            return String::new();
        }
        let mut out = String::from("<nav class=\"pager\">");
        if self.page > 1 {
            out.push_str(&format!(
                "<a href=\"{}{}\">&laquo; prev</a>",
                esc(&self.base_url),
                self.prev_query,
            ));
        } else {
            out.push_str("<span class=\"muted\">&laquo; prev</span>");
        }
        out.push_str(&format!(
            " <span class=\"muted\">page {} of {} ({} items)</span> ",
            self.page, self.pages, total
        ));
        if self.page < self.pages {
            out.push_str(&format!(
                "<a href=\"{}{}\">next &raquo;</a>",
                esc(&self.base_url),
                self.next_query,
            ));
        } else {
            out.push_str("<span class=\"muted\">next &raquo;</span>");
        }
        out.push_str("</nav>");
        out
    }
}

/// Render a compact filter bar for a list page: a search input plus
/// year chips (skipped when `years` is empty). Submitting the search
/// preserves the current `year=` filter but resets `page=` since page
/// numbers rarely make sense after the row set changes.
fn filter_bar(base_url: &str, query: &ListQuery, years: &[String]) -> String {
    let mut out = String::from("<div class=\"filter-bar\">");
    let hidden_year = query
        .year
        .as_deref()
        .map(|y| format!("<input type=\"hidden\" name=\"year\" value=\"{}\">", esc(y),))
        .unwrap_or_default();
    let q_value = query.q.as_deref().unwrap_or("");
    out.push_str(&format!(
        "<form method=\"get\" action=\"{}\" class=\"search\">\
         {hidden_year}<input type=\"search\" name=\"q\" value=\"{}\" \
         placeholder=\"search\" autocomplete=\"off\">\
         <button type=\"submit\">go</button></form>",
        esc(base_url),
        esc(q_value),
    ));
    if !years.is_empty() {
        out.push_str("<div class=\"year-chips\">");
        // "all" chip clears the year filter but keeps any active search.
        let all_href = {
            let q = ListQuery {
                page: None,
                q: query.q.clone(),
                year: None,
            };
            format!("{}{}", base_url, q.to_query_string(None))
        };
        let all_active = query.year.is_none();
        out.push_str(&format!(
            "<a href=\"{}\" class=\"{}\">all</a>",
            esc(&all_href),
            if all_active { "chip active" } else { "chip" },
        ));
        for year in years {
            let href = {
                let q = ListQuery {
                    page: None,
                    q: query.q.clone(),
                    year: Some(year.clone()),
                };
                format!("{}{}", base_url, q.to_query_string(None))
            };
            let active = query.year.as_deref() == Some(year.as_str());
            out.push_str(&format!(
                "<a href=\"{}\" class=\"{}\">{}</a>",
                esc(&href),
                if active { "chip active" } else { "chip" },
                esc(year),
            ));
        }
        out.push_str("</div>");
    }
    out.push_str("</div>");
    out
}

/// True when `query` has no substring filter, or when `haystack`
/// contains that substring (case-insensitive). `haystack` should
/// concatenate every user-visible field on a row so the match feels
/// like "find anywhere on this line".
fn matches_search(query: &ListQuery, haystack: &str) -> bool {
    let Some(q) = &query.q else {
        return true;
    };
    haystack
        .to_ascii_lowercase()
        .contains(&q.to_ascii_lowercase())
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
                vendor_url: None,
                href: format!("/events/{name}"),
                blobs: Vec::new(),
            });
        }
    }

    if let Some(dir) = state.bills_dir() {
        for (year, slug, value) in walk_year_json(dir)? {
            let payee = pick_str(&value, &["payee", "accountName"]).unwrap_or_default();
            let invoice = pick_str(&value, &["invoiceNumber", "identifier"]).unwrap_or_default();
            let date = pick_str(
                &value,
                &[
                    "receivedAt",
                    "dueDate",
                    "paymentDueDate",
                    "date",
                    "issueDate",
                ],
            )
            .and_then(|d| parse_any_date(&d))
            .unwrap_or_else(|| mtime_date(&dir.join(&year).join(format!("{slug}.json"))));
            let blobs = sibling_blobs(dir, &year, &slug)
                .into_iter()
                .map(|(label, name)| (label, format!("/bills/{year}/{name}")))
                .collect();
            items.push(FeedItem {
                date,
                kind: "bill",
                title: payee,
                subtitle: invoice,
                vendor_url: vendor_url(&value),
                href: format!("/bills/{year}/{slug}.json"),
                blobs,
            });
        }
    }

    if let Some(dir) = state.parcels_dir() {
        for (name, value) in walk_flat_json(dir)? {
            let tracking = pick_str(&value, &["trackingNumber", "identifier"]).unwrap_or_default();
            let status = pick_str(&value, &["deliveryStatus"]).unwrap_or_default();
            // Prefer the ETA over receivedAt so an OutForDelivery
            // parcel arriving next Tuesday shows in Upcoming rather
            // than sinking into Recent under yesterday's status update.
            let date = pick_str(
                &value,
                &[
                    "actualDeliveryTime",
                    "expectedArrivalUntil",
                    "expectedArrivalFrom",
                    "receivedAt",
                ],
            )
            .and_then(|d| parse_any_date(&d))
            .unwrap_or_else(|| mtime_date(&dir.join(&name)));
            items.push(FeedItem {
                date,
                kind: "parcel",
                title: tracking,
                subtitle: status,
                vendor_url: vendor_url(&value),
                href: format!("/parcels/{name}"),
                blobs: Vec::new(),
            });
        }
    }

    if let Some(dir) = state.receipts_dir() {
        for (year, slug, value) in walk_year_json(dir)? {
            let merchant = pick_str(&value, &["merchant", "seller"]).unwrap_or_default();
            let order = pick_str(&value, &["orderNumber", "identifier"]).unwrap_or_default();
            let date = pick_str(&value, &["receivedAt", "orderDate", "date"])
                .and_then(|d| parse_any_date(&d))
                .unwrap_or_else(|| mtime_date(&dir.join(&year).join(format!("{slug}.json"))));
            let blobs = sibling_blobs(dir, &year, &slug)
                .into_iter()
                .map(|(label, name)| (label, format!("/receipts/{year}/{name}")))
                .collect();
            items.push(FeedItem {
                date,
                kind: "receipt",
                title: merchant,
                subtitle: order,
                vendor_url: vendor_url(&value),
                href: format!("/receipts/{year}/{slug}.json"),
                blobs,
            });
        }
    }

    if let Some(dir) = state.subscriptions_dir() {
        for (name, value) in walk_flat_json(dir)? {
            let display = pick_str(&value, &["name", "provider"]).unwrap_or_default();
            let renewal = pick_str(&value, &["renewalDate", "nextPaymentDate"]);
            let date = pick_str(&value, &["receivedAt"])
                .as_deref()
                .and_then(parse_any_date)
                .or_else(|| renewal.as_deref().and_then(parse_any_date))
                .unwrap_or_else(|| mtime_date(&dir.join(&name)));
            items.push(FeedItem {
                date,
                kind: "subscription",
                title: display,
                subtitle: renewal.unwrap_or_default(),
                vendor_url: vendor_url(&value),
                href: format!("/subscriptions/{name}"),
                blobs: Vec::new(),
            });
        }
    }

    if let Some(dir) = state.reservations_dir() {
        for (year, slug, value) in walk_year_json(dir)? {
            let date = reservation_date(&value)
                .and_then(|d| parse_any_date(&d))
                .unwrap_or_else(|| mtime_date(&dir.join(&year).join(format!("{slug}.json"))));
            items.push(FeedItem {
                date,
                kind: "reservation",
                title: reservation_provider(&value).unwrap_or_default(),
                subtitle: reservation_number(&value).unwrap_or_default(),
                vendor_url: vendor_url(&value),
                href: format!("/reservations/{year}/{slug}.json"),
                blobs: Vec::new(),
            });
        }
    }

    if let Some(dir) = state.tickets_dir() {
        for (year, group) in group_ticket_files(dir)? {
            let year_num: i32 = year.parse().unwrap_or(0);
            for (slug, files) in group {
                let meta = files
                    .iter()
                    .find(|f| f.is_meta)
                    .and_then(|f| read_json(&f.path).ok());
                let date = meta
                    .as_ref()
                    .and_then(|m| pick_str(m, &["receivedAt"]))
                    .and_then(|s| parse_any_date(&s))
                    .or_else(|| files.iter().filter_map(|f| mtime_date_opt(&f.path)).max())
                    .or_else(|| NaiveDate::from_ymd_opt(year_num, 1, 1))
                    .unwrap_or_else(|| Utc::now().date_naive());
                let title = meta
                    .as_ref()
                    .and_then(|m| pick_str(m, &["provider"]))
                    .unwrap_or_else(|| slug.clone());
                let subtitle = meta
                    .as_ref()
                    .and_then(|m| pick_str(m, &["reservationNumber", "identifier"]))
                    .unwrap_or_else(|| year.clone());
                let (href, blobs) = ticket_links(&year, &files);
                items.push(FeedItem {
                    date,
                    kind: "ticket",
                    title,
                    subtitle,
                    vendor_url: None,
                    href,
                    blobs,
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

async fn list_bills(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.bills_dir(), "bills")?;
    // (sort_date, year, slug, cells): sort_date is `None` for
    // date-less payloads so they sink to the bottom.
    let mut rows: Vec<(Option<NaiveDate>, String, String, String)> = Vec::new();
    let mut years_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (year, slug, value) in walk_year_json(dir)? {
        years_seen.insert(year.clone());
        if let Some(y) = &query.year
            && &year != y
        {
            continue;
        }
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
        // Search haystack: every visible column so `?q=` matches
        // anywhere on the line.
        let haystack = format!(
            "{} {} {} {}",
            payee.as_deref().unwrap_or(""),
            invoice.as_deref().unwrap_or(""),
            due.as_deref().unwrap_or(""),
            amount,
        );
        if !matches_search(&query, &haystack) {
            continue;
        }
        let href = state.url(&format!("/bills/{}/{}.json", year, slug));
        let view_href = state.url(&format!("/bills/{year}/{slug}/view"));
        let vendor = vendor_url(&value);
        let blobs: Vec<(String, String)> = sibling_blobs(dir, &year, &slug)
            .into_iter()
            .map(|(label, name)| (label, state.url(&format!("/bills/{year}/{name}"))))
            .collect();
        let sort_date = due.as_deref().and_then(parse_any_date);
        let due_display = due.unwrap_or_else(|| year.clone());
        let cells = format!(
            "<td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&view_href),
            esc(&payee.unwrap_or_default()),
            esc(&invoice.unwrap_or_default()),
            esc(&short_date(&due_display)),
            esc(&amount),
            links_cell(&href, &blobs, vendor.as_deref()),
        );
        rows.push((sort_date, year, slug, cells));
    }
    rows.sort_by(|a, b| {
        b.0.cmp(&a.0) // newest first, None sinks
            .then(b.1.cmp(&a.1))
            .then(a.2.cmp(&b.2))
    });
    let base_url = state.url("/bills");
    let years: Vec<String> = years_seen.into_iter().rev().collect();
    let bar = filter_bar(&base_url, &query, &years);
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Bills",
            &format!("{bar}<div class=\"empty\">no bills</div>"),
        )));
    }
    let total = rows.len();
    let (page_rows, pager) = paginate(&rows, &query, &base_url);
    let body = format!(
        "{bar}<table><thead><tr><th>payee</th><th>invoice</th><th>due</th><th>amount</th><th></th></tr></thead>\
         <tbody>{rows}</tbody></table>{pager}",
        rows = page_rows
            .iter()
            .map(|(_, _, _, cells)| format!("<tr>{cells}</tr>"))
            .collect::<Vec<_>>()
            .join(""),
        pager = pager.render(total),
    );
    Ok(Html(page(&state, "Bills", &body)))
}

async fn get_bill(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.bills_dir(), "bills")?;
    serve_shard_file(dir, &year, &name)
}

async fn view_bill(
    State(state): State<Arc<AppState>>,
    UrlPath((year, slug)): UrlPath<(String, String)>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.bills_dir(), "bills")?;
    let value = read_shard_json(dir, &year, &slug)?;
    let title = pick_str(&value, &["payee", "accountName"]).unwrap_or_else(|| slug.clone());
    let subtitle = pick_str(&value, &["invoiceNumber", "identifier"]).unwrap_or_default();
    let fields = [
        ("Payee", pick_str(&value, &["payee", "accountName"])),
        (
            "Invoice number",
            pick_str(&value, &["invoiceNumber", "identifier"]),
        ),
        (
            "Due",
            pick_str(&value, &["dueDate", "paymentDueDate", "date"]).map(|s| short_date(&s)),
        ),
        (
            "Issued",
            pick_str(&value, &["issueDate", "date"]).map(|s| short_date(&s)),
        ),
        (
            "Amount",
            value
                .get("totalPaymentDue")
                .map(format_price)
                .filter(|s| !s.is_empty()),
        ),
        (
            "Received",
            pick_str(&value, &["receivedAt"]).map(|s| short_date(&s)),
        ),
    ];
    let blobs: Vec<(String, String)> = sibling_blobs(dir, &year, &slug)
        .into_iter()
        .map(|(label, name)| (label, state.url(&format!("/bills/{year}/{name}"))))
        .collect();
    Ok(Html(page(
        &state,
        &title,
        &detail_view(
            &subtitle,
            &fields,
            &state.url(&format!("/bills/{year}/{slug}.json")),
            &blobs,
            vendor_url(&value).as_deref(),
        ),
    )))
}

async fn list_parcels(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.parcels_dir(), "parcels")?;
    let mut rows: Vec<(Option<NaiveDate>, String, String)> = Vec::new();
    for (name, value) in walk_flat_json(dir)? {
        let tracking = pick_str(&value, &["trackingNumber", "identifier"]).unwrap_or_default();
        let status = pick_str(&value, &["deliveryStatus"]).unwrap_or_default();
        let carrier = value
            .get("provider")
            .and_then(|p| p.get("name").or_else(|| p.get("@id")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // For delivered/returned parcels, extractors often don't fill
        // `actualDeliveryTime`; the `receivedAt` of the delivery email
        // is a close proxy so the row doesn't render a blank date. If
        // the record has no `receivedAt` either (older backfilled
        // parcels), fall back to the newest history entry's `seen_at`
        // - roughly the pipeline's last touch, close enough for a
        // list row.
        let terminal = matches!(
            status.as_str(),
            "OrderDelivered" | "Delivered" | "OrderReturned" | "Returned" | "ReturnedToSender"
        );
        let mut due_keys: Vec<&str> = vec![
            "actualDeliveryTime",
            "expectedArrivalUntil",
            "expectedArrivalFrom",
        ];
        if terminal {
            due_keys.push("receivedAt");
        }
        let due = pick_str(&value, &due_keys)
            .or_else(|| terminal.then(|| last_history_seen_at(&value)).flatten())
            .unwrap_or_default();
        let haystack = format!("{tracking} {carrier} {status} {due}");
        if !matches_search(&query, &haystack) {
            continue;
        }
        let sort_date = pick_str(
            &value,
            &[
                "actualDeliveryTime",
                "expectedArrivalUntil",
                "receivedAt",
                "expectedArrivalFrom",
            ],
        )
        .or_else(|| last_history_seen_at(&value))
        .as_deref()
        .and_then(parse_any_date);
        let vendor = vendor_url(&value);
        let slug = name.strip_suffix(".json").unwrap_or(&name);
        let view_href = state.url(&format!("/parcels/{slug}/view"));
        let cells = format!(
            "<td><a href=\"{}\">{}</a></td><td><span class=\"badge\">{}</span></td>\
             <td>{}</td><td>{}</td><td>{}</td>",
            esc(&view_href),
            esc(&tracking),
            esc(&carrier),
            esc(&status),
            esc(&short_date(&due)),
            links_cell(
                &state.url(&format!("/parcels/{name}")),
                &[],
                vendor.as_deref()
            ),
        );
        rows.push((sort_date, name, cells));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let base_url = state.url("/parcels");
    let bar = filter_bar(&base_url, &query, &[]);
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Parcels",
            &format!("{bar}<div class=\"empty\">no parcels</div>"),
        )));
    }
    let total = rows.len();
    let (page_rows, pager) = paginate(&rows, &query, &base_url);
    let body = format!(
        "{bar}<table><thead><tr><th>tracking</th><th>carrier</th><th>status</th><th>date</th><th></th></tr></thead>\
         <tbody>{rows}</tbody></table>{pager}",
        rows = page_rows
            .iter()
            .map(|(_, _, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join(""),
        pager = pager.render(total),
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

async fn view_parcel(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.parcels_dir(), "parcels")?;
    let file = if name.ends_with(".json") {
        name.clone()
    } else {
        format!("{name}.json")
    };
    let value = read_flat_json(dir, &file)?;
    let tracking =
        pick_str(&value, &["trackingNumber", "identifier"]).unwrap_or_else(|| name.clone());
    let carrier = value
        .get("provider")
        .and_then(|p| p.get("name").or_else(|| p.get("@id")))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let fields = [
        ("Tracking number", Some(tracking.clone())),
        ("Carrier", (!carrier.is_empty()).then_some(carrier)),
        ("Status", pick_str(&value, &["deliveryStatus"])),
        (
            "Expected",
            pick_str(&value, &["expectedArrivalUntil", "expectedArrivalFrom"])
                .map(|s| short_date(&s)),
        ),
        (
            "Delivered",
            pick_str(&value, &["actualDeliveryTime"]).map(|s| short_date(&s)),
        ),
        (
            "Received",
            pick_str(&value, &["receivedAt"]).map(|s| short_date(&s)),
        ),
    ];
    Ok(Html(page(
        &state,
        &tracking,
        &detail_view(
            "",
            &fields,
            &state.url(&format!("/parcels/{file}")),
            &[],
            vendor_url(&value).as_deref(),
        ),
    )))
}

async fn list_receipts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    let mut rows: Vec<(Option<NaiveDate>, String, String, String)> = Vec::new();
    let mut years_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (year, slug, value) in walk_year_json(dir)? {
        years_seen.insert(year.clone());
        if let Some(y) = &query.year
            && &year != y
        {
            continue;
        }
        let merchant = pick_str(&value, &["merchant", "seller"]).unwrap_or_default();
        let order = pick_str(&value, &["orderNumber", "identifier"]).unwrap_or_default();
        let date_raw = pick_str(&value, &["orderDate", "date"]);
        let sort_date = date_raw.as_deref().and_then(parse_any_date);
        let date_display = date_raw.unwrap_or_else(|| year.clone());
        let total_str = value
            .get("priceSpecification")
            .map(format_price)
            .unwrap_or_default();
        let haystack = format!("{} {} {} {}", merchant, order, date_display, total_str,);
        if !matches_search(&query, &haystack) {
            continue;
        }
        let vendor = vendor_url(&value);
        let view_href = state.url(&format!("/receipts/{year}/{slug}/view"));
        let blobs: Vec<(String, String)> = sibling_blobs(dir, &year, &slug)
            .into_iter()
            .map(|(label, name)| (label, state.url(&format!("/receipts/{year}/{name}"))))
            .collect();
        let cells = format!(
            "<td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&view_href),
            esc(&merchant),
            esc(&order),
            esc(&short_date(&date_display)),
            esc(&total_str),
            links_cell(
                &state.url(&format!("/receipts/{year}/{slug}.json")),
                &blobs,
                vendor.as_deref(),
            ),
        );
        rows.push((sort_date, year, slug, cells));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let base_url = state.url("/receipts");
    let years: Vec<String> = years_seen.into_iter().rev().collect();
    let bar = filter_bar(&base_url, &query, &years);
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Receipts",
            &format!("{bar}<div class=\"empty\">no receipts</div>"),
        )));
    }
    let total = rows.len();
    let (page_rows, pager) = paginate(&rows, &query, &base_url);
    let body = format!(
        "{bar}<table><thead><tr><th>merchant</th><th>order</th><th>date</th><th>total</th><th></th></tr></thead>\
         <tbody>{rows}</tbody></table>{pager}",
        rows = page_rows
            .iter()
            .map(|(_, _, _, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join(""),
        pager = pager.render(total),
    );
    Ok(Html(page(&state, "Receipts", &body)))
}

async fn get_receipt(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    serve_shard_file(dir, &year, &name)
}

async fn view_receipt(
    State(state): State<Arc<AppState>>,
    UrlPath((year, slug)): UrlPath<(String, String)>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    let value = read_shard_json(dir, &year, &slug)?;
    let title = pick_str(&value, &["merchant", "seller"]).unwrap_or_else(|| slug.clone());
    let order = pick_str(&value, &["orderNumber", "identifier"]).unwrap_or_default();
    let fields = [
        ("Merchant", pick_str(&value, &["merchant", "seller"])),
        (
            "Order number",
            pick_str(&value, &["orderNumber", "identifier"]),
        ),
        (
            "Ordered",
            pick_str(&value, &["orderDate", "date"]).map(|s| short_date(&s)),
        ),
        (
            "Total",
            value
                .get("priceSpecification")
                .map(format_price)
                .filter(|s| !s.is_empty()),
        ),
        (
            "Received",
            pick_str(&value, &["receivedAt"]).map(|s| short_date(&s)),
        ),
    ];
    let blobs: Vec<(String, String)> = sibling_blobs(dir, &year, &slug)
        .into_iter()
        .map(|(label, name)| (label, state.url(&format!("/receipts/{year}/{name}"))))
        .collect();
    Ok(Html(page(
        &state,
        &title,
        &detail_view(
            &order,
            &fields,
            &state.url(&format!("/receipts/{year}/{slug}.json")),
            &blobs,
            vendor_url(&value).as_deref(),
        ),
    )))
}

async fn list_subscriptions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.subscriptions_dir(), "subscriptions")?;
    let today = Utc::now().date_naive();
    // (sort_date, active, name, cells): sort active first, then newest.
    let mut rows: Vec<(bool, Option<NaiveDate>, String, String)> = Vec::new();
    for (name, value) in walk_flat_json(dir)? {
        let display = pick_str(&value, &["name", "provider"]).unwrap_or_default();
        let renewal = pick_str(&value, &["renewalDate", "nextPaymentDate"]).unwrap_or_default();
        let price = format_price(&value);
        let started = pick_str(&value, &["orderDate", "receivedAt"]).unwrap_or_default();
        let sort_date = parse_any_date(&started);
        let received = pick_str(&value, &["receivedAt"])
            .as_deref()
            .and_then(parse_any_date);
        let duration = pick_str(&value, &["subscriptionDuration"]);
        let active = is_subscription_active(received, duration.as_deref(), today);
        let haystack = format!("{display} {started} {renewal} {price}");
        if !matches_search(&query, &haystack) {
            continue;
        }
        let status_html = if active {
            "<span class=\"badge subscription\">active</span>".to_string()
        } else {
            "<span class=\"badge muted-badge\">inactive</span>".to_string()
        };
        let vendor = vendor_url(&value);
        let slug = name.strip_suffix(".json").unwrap_or(&name);
        let view_href = state.url(&format!("/subscriptions/{slug}/view"));
        let cells = format!(
            "<td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&view_href),
            esc(&display),
            status_html,
            esc(&short_date(&started)),
            esc(&short_date(&renewal)),
            esc(&price),
            links_cell(
                &state.url(&format!("/subscriptions/{name}")),
                &[],
                vendor.as_deref(),
            ),
        );
        rows.push((active, sort_date, name, cells));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let base_url = state.url("/subscriptions");
    let bar = filter_bar(&base_url, &query, &[]);
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Subscriptions",
            &format!("{bar}<div class=\"empty\">no subscriptions</div>"),
        )));
    }
    let body = format!(
        "{bar}<table><thead><tr><th>name</th><th>status</th><th>started</th><th>renews</th>\
         <th>price</th><th></th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.into_iter()
            .map(|(_, _, _, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join("")
    );
    Ok(Html(page(&state, "Subscriptions", &body)))
}

/// Heuristic active/inactive flag for a subscription. Active while
/// `received_at` is within twice the subscription's own cycle (so we'd
/// expect at least one more renewal email by now if it were still
/// running). Falls back to a 60-day window when the record has no
/// parseable duration.
fn is_subscription_active(
    received: Option<NaiveDate>,
    duration: Option<&str>,
    today: NaiveDate,
) -> bool {
    let Some(received) = received else {
        return true; // no signal to declare it inactive on
    };
    let stale_after_days = duration
        .and_then(parse_iso_duration_days)
        .map(|d| d.saturating_mul(2))
        .unwrap_or(60);
    let age = today.signed_duration_since(received).num_days();
    age <= i64::from(stale_after_days)
}

/// `(active_count, total_count)` for the subscriptions overview card.
/// Applies the same activity heuristic as the list page so the
/// numbers match.
fn count_active_subscriptions(dir: &Path) -> Result<(usize, usize)> {
    let today = Utc::now().date_naive();
    let mut active = 0;
    let mut total = 0;
    for (_, value) in walk_flat_json(dir)? {
        total += 1;
        let received = pick_str(&value, &["receivedAt"])
            .as_deref()
            .and_then(parse_any_date);
        let duration = pick_str(&value, &["subscriptionDuration"]);
        if is_subscription_active(received, duration.as_deref(), today) {
            active += 1;
        }
    }
    Ok((active, total))
}

/// Approximate an ISO 8601 duration (`P1M`, `P1Y`, `P7D`, ...) as a
/// day count. Only supports the single-designator forms schema.org
/// subscription payloads actually use; anything more elaborate returns
/// `None` and callers fall back to a default window.
fn parse_iso_duration_days(iso: &str) -> Option<u32> {
    let rest = iso.strip_prefix('P')?;
    if rest.is_empty() {
        return None;
    }
    let (num_str, unit) = rest.split_at(rest.len() - 1);
    let n: u32 = num_str.parse().ok()?;
    match unit {
        "D" => Some(n),
        "W" => Some(n.saturating_mul(7)),
        "M" => Some(n.saturating_mul(30)),
        "Y" => Some(n.saturating_mul(365)),
        _ => None,
    }
}

/// Format a subscription/bill/receipt `price` field as `"1.59 GBP"`.
/// Renders `"free"` when the price parses as exactly zero, and an empty
/// string when the payload has no numeric price at all.
/// The `seen_at` timestamp of the last history entry on a parcel
/// record, if any. Records filed before `receivedAt` stamping was
/// added carry only this pipeline timestamp; the parcel list uses it
/// as a last-resort date column so terminal entries don't render
/// blank.
fn last_history_seen_at(value: &Value) -> Option<String> {
    value
        .get("history")
        .and_then(Value::as_array)
        .and_then(|arr| arr.last())
        .and_then(|entry| entry.get("seen_at"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

fn format_price(value: &Value) -> String {
    let Some(raw) = value.get("price") else {
        return String::new();
    };
    let amount = raw
        .as_f64()
        .or_else(|| raw.as_str().and_then(|s| s.parse().ok()));
    let Some(amount) = amount else {
        return raw
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| raw.to_string());
    };
    if amount == 0.0 {
        return "free".into();
    }
    let currency = value
        .get("priceCurrency")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if currency.is_empty() {
        format!("{amount}")
    } else {
        format!("{amount} {currency}")
    }
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

async fn view_subscription(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.subscriptions_dir(), "subscriptions")?;
    let file = if name.ends_with(".json") {
        name.clone()
    } else {
        format!("{name}.json")
    };
    let value = read_flat_json(dir, &file)?;
    let title = pick_str(&value, &["name", "provider"]).unwrap_or_else(|| name.clone());
    let fields = [
        ("Name", pick_str(&value, &["name"])),
        ("Provider", pick_str(&value, &["provider"])),
        (
            "Started",
            pick_str(&value, &["orderDate", "receivedAt"]).map(|s| short_date(&s)),
        ),
        (
            "Renews",
            pick_str(&value, &["renewalDate", "nextPaymentDate"]).map(|s| short_date(&s)),
        ),
        ("Cycle", pick_str(&value, &["subscriptionDuration"])),
        (
            "Price",
            Some(format_price(&value)).filter(|s| !s.is_empty()),
        ),
    ];
    Ok(Html(page(
        &state,
        &title,
        &detail_view(
            "",
            &fields,
            &state.url(&format!("/subscriptions/{file}")),
            &[],
            vendor_url(&value).as_deref(),
        ),
    )))
}

/// Name of a schema.org node that may be a bare string or an object
/// with a `name` (or, for airlines, an `iataCode`). Mirrors the
/// `Named` handling in [`crate::targets::reservations`].
fn named(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return (!s.trim().is_empty()).then(|| s.to_string());
    }
    pick_str(value, &["name", "iataCode"])
}

/// Who a reservation is with. Same precedence as the filing target:
/// the airline, then a provider (top-level or nested under
/// `reservationFor` as train records emit), then the broker, then the
/// name of the thing reserved (hotel, venue).
fn reservation_provider(value: &Value) -> Option<String> {
    let for_ = value.get("reservationFor");
    for_.and_then(|f| f.get("airline"))
        .and_then(named)
        .or_else(|| value.get("provider").and_then(named))
        .or_else(|| for_.and_then(|f| f.get("provider")).and_then(named))
        .or_else(|| value.get("broker").and_then(named))
        .or_else(|| for_.and_then(|f| f.get("name")).and_then(named))
}

/// Booking reference.
fn reservation_number(value: &Value) -> Option<String> {
    pick_str(value, &["reservationNumber", "reservationId", "identifier"])
}

/// The date this reservation happens on, most specific first. Prefers
/// the trip's own date over `receivedAt` so a booking made months
/// ahead still sorts as upcoming.
fn reservation_date(value: &Value) -> Option<String> {
    let for_ = value.get("reservationFor");
    for_.and_then(|f| pick_str(f, &["departureTime", "startDate", "doorTime"]))
        .or_else(|| pick_str(value, &["checkinTime", "startTime", "receivedAt"]))
}

async fn list_reservations(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.reservations_dir(), "reservations")?;
    let mut rows: Vec<(Option<NaiveDate>, String, String, String)> = Vec::new();
    let mut years_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (year, slug, value) in walk_year_json(dir)? {
        years_seen.insert(year.clone());
        if let Some(y) = &query.year
            && &year != y
        {
            continue;
        }
        let provider = reservation_provider(&value).unwrap_or_default();
        let number = reservation_number(&value).unwrap_or_default();
        let under = value.get("underName").and_then(named).unwrap_or_default();
        let date_raw = reservation_date(&value).unwrap_or_default();
        let sort_date = parse_any_date(&date_raw);
        let route = reservation_route(&value).unwrap_or_default();
        let haystack = format!("{provider} {number} {route} {under} {date_raw}");
        if !matches_search(&query, &haystack) {
            continue;
        }
        let view_href = state.url(&format!("/reservations/{year}/{slug}/view"));
        let cells = format!(
            "<td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&view_href),
            esc(&provider),
            esc(&number),
            esc(&route),
            esc(&under),
            esc(&short_date(&date_raw)),
            links_cell(
                &state.url(&format!("/reservations/{year}/{slug}.json")),
                &[],
                vendor_url(&value).as_deref(),
            ),
        );
        rows.push((sort_date, year, slug, cells));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let base_url = state.url("/reservations");
    let years: Vec<String> = years_seen.into_iter().rev().collect();
    let bar = filter_bar(&base_url, &query, &years);
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Reservations",
            &format!("{bar}<div class=\"empty\">no reservations</div>"),
        )));
    }
    let total = rows.len();
    let (page_rows, pager) = paginate(&rows, &query, &base_url);
    let body = format!(
        "{bar}<table><thead><tr><th>provider</th><th>reference</th><th>route</th>\
         <th>name</th><th>date</th><th></th></tr></thead>\
         <tbody>{rows}</tbody></table>{pager}",
        rows = page_rows
            .iter()
            .map(|(_, _, _, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join(""),
        pager = pager.render(total),
    );
    Ok(Html(page(&state, "Reservations", &body)))
}

/// Compact route description for a reservation. Flights render as
/// `LHR -> AMS`; train and coach legs use station / stop names since
/// IATA-style codes aren't standardised for rail. Anything else
/// returns `None`.
fn reservation_route(value: &Value) -> Option<String> {
    let for_ = value.get("reservationFor")?;
    let kind = for_.get("@type").and_then(|t| t.as_str()).unwrap_or("");
    let (depart, arrive) = match kind {
        "Flight" => (
            for_.get("departureAirport")
                .and_then(|a| pick_str(a, &["iataCode", "name"])),
            for_.get("arrivalAirport")
                .and_then(|a| pick_str(a, &["iataCode", "name"])),
        ),
        "TrainTrip" | "BusTrip" => (
            for_.get("departureStation")
                .or_else(|| for_.get("departureBusStop"))
                .and_then(|s| pick_str(s, &["name"])),
            for_.get("arrivalStation")
                .or_else(|| for_.get("arrivalBusStop"))
                .and_then(|s| pick_str(s, &["name"])),
        ),
        _ => return None,
    };
    match (depart, arrive) {
        (Some(a), Some(b)) => Some(format!("{a} -> {b}")),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

async fn get_reservation(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.reservations_dir(), "reservations")?;
    serve_shard_file(dir, &year, &name)
}

async fn view_reservation(
    State(state): State<Arc<AppState>>,
    UrlPath((year, slug)): UrlPath<(String, String)>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.reservations_dir(), "reservations")?;
    let value = read_shard_json(dir, &year, &slug)?;
    let provider = reservation_provider(&value).unwrap_or_else(|| slug.clone());
    let number = reservation_number(&value).unwrap_or_default();
    let route = reservation_route(&value);
    let fields = [
        ("Provider", Some(provider.clone())),
        ("Reference", (!number.is_empty()).then_some(number.clone())),
        ("Route", route),
        ("Passenger", value.get("underName").and_then(named)),
        ("Date", reservation_date(&value).map(|s| short_date(&s))),
        (
            "Received",
            pick_str(&value, &["receivedAt"]).map(|s| short_date(&s)),
        ),
        ("Ticket number", pick_str(&value, &["ticketNumber"])),
    ];
    Ok(Html(page(
        &state,
        &provider,
        &detail_view(
            &number,
            &fields,
            &state.url(&format!("/reservations/{year}/{slug}.json")),
            &[],
            vendor_url(&value).as_deref(),
        ),
    )))
}

async fn list_tickets(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.tickets_dir(), "tickets")?;
    let mut rows: Vec<(Option<NaiveDate>, String, String, String)> = Vec::new();
    let mut years_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (year, group) in group_ticket_files(dir)? {
        years_seen.insert(year.clone());
        if let Some(y) = &query.year
            && &year != y
        {
            continue;
        }
        for (slug, files) in group {
            let size: u64 = files.iter().map(|f| f.size).sum();
            let meta = files
                .iter()
                .find(|f| f.is_meta)
                .and_then(|f| read_json(&f.path).ok());
            let provider = meta
                .as_ref()
                .and_then(|m| pick_str(m, &["provider"]))
                .unwrap_or_default();
            let reference = meta
                .as_ref()
                .and_then(|m| pick_str(m, &["reservationNumber", "identifier"]))
                .unwrap_or_default();
            let received = meta
                .as_ref()
                .and_then(|m| pick_str(m, &["receivedAt"]))
                .and_then(|s| parse_any_date(&s))
                .or_else(|| files.iter().filter_map(|f| mtime_date_opt(&f.path)).max());
            let haystack = format!("{provider} {reference} {slug}");
            if !matches_search(&query, &haystack) {
                continue;
            }
            let received_display = received.map(|d| d.to_string()).unwrap_or_default();
            let downloads = files
                .iter()
                .map(|f| {
                    format!(
                        "<a href=\"{}\">{}</a>",
                        esc(&state.url(&format!("/tickets/{year}/{}", f.name))),
                        esc(f.label()),
                    )
                })
                .collect::<Vec<_>>()
                .join(" &middot; ");
            let view_href = state.url(&format!("/tickets/{year}/{slug}/view"));
            let title_display = if !provider.is_empty() {
                provider.clone()
            } else {
                slug.clone()
            };
            let cells = format!(
                "<td>{}</td><td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
                esc(&received_display),
                esc(&view_href),
                esc(&title_display),
                esc(&reference),
                esc(&slug),
                human_size(size),
                downloads,
            );
            rows.push((received, year.clone(), slug, cells));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let base_url = state.url("/tickets");
    let years: Vec<String> = years_seen.into_iter().rev().collect();
    let bar = filter_bar(&base_url, &query, &years);
    if rows.is_empty() {
        return Ok(Html(page(
            &state,
            "Tickets",
            &format!("{bar}<div class=\"empty\">no tickets</div>"),
        )));
    }
    let body = format!(
        "{bar}<table><thead><tr><th>received</th><th>provider</th><th>reference</th>\
         <th>slug</th><th>size</th><th></th></tr></thead>\
         <tbody>{}</tbody></table>",
        rows.into_iter()
            .map(|(_, _, _, c)| format!("<tr>{c}</tr>"))
            .collect::<Vec<_>>()
            .join("")
    );
    Ok(Html(page(&state, "Tickets", &body)))
}

/// One file that belongs to a ticket group under a `<year>/` shard.
struct TicketFile {
    /// Filename as stored on disk.
    name: String,
    /// Full path, kept so callers can read mtime without going back to
    /// the directory.
    path: PathBuf,
    /// File size in bytes.
    size: u64,
    /// True when the filename is `<slug>.meta.json`, the metadata
    /// sidecar written beside each ticket blob.
    is_meta: bool,
}

impl TicketFile {
    /// Short label used in the type column ("json" for the sidecar,
    /// otherwise the file extension lowercased, e.g. "pdf", "pkpass").
    fn label(&self) -> &str {
        if self.is_meta {
            "json"
        } else {
            Path::new(&self.name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
        }
    }
}

/// Ticket slug for a filename in a year shard: `<slug>.meta.json` maps
/// to `<slug>`, other extensions map to `<file_stem>`. Returns `None`
/// for files without a usable extension.
fn ticket_slug(name: &str) -> Option<(String, bool)> {
    if let Some(stem) = name.strip_suffix(".meta.json") {
        return Some((stem.to_string(), true));
    }
    let (stem, _ext) = name.rsplit_once('.')?;
    Some((stem.to_string(), false))
}

/// `(slug, files_for_that_slug)`: one entry per ticket group within a
/// year shard.
type TicketSlugGroup = (String, Vec<TicketFile>);

/// `(year, groups_in_that_year)`: one entry per `<year>/` shard.
type TicketYearGroup = (String, Vec<TicketSlugGroup>);

/// Walk `<tickets_dir>/<year>/*`, grouping files by slug. Returns a
/// deterministic order: years descending, slugs ascending, and within
/// each slug the metadata sidecar (if any) first followed by blobs
/// sorted by extension.
/// Count ticket *groups* (one per slug) under a tickets directory.
/// Used for the overview card so a ticket with both a `.pdf` and its
/// `.meta.json` sidecar counts as one, matching what the list page
/// renders.
fn count_ticket_groups(dir: &Path) -> Result<usize> {
    Ok(group_ticket_files(dir)?
        .into_iter()
        .map(|(_, groups)| groups.len())
        .sum())
}

fn group_ticket_files(dir: &Path) -> Result<Vec<TicketYearGroup>> {
    let mut years: Vec<TicketYearGroup> = Vec::new();
    for entry in read_dir_or_empty(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let year = entry.file_name().to_string_lossy().into_owned();
        let mut by_slug: std::collections::BTreeMap<String, Vec<TicketFile>> =
            std::collections::BTreeMap::new();
        for inner in fs::read_dir(entry.path())? {
            let inner = inner?;
            if !inner.file_type()?.is_file() {
                continue;
            }
            let name = inner.file_name().to_string_lossy().into_owned();
            let Some((slug, is_meta)) = ticket_slug(&name) else {
                continue;
            };
            let size = inner.metadata().map(|m| m.len()).unwrap_or(0);
            by_slug.entry(slug).or_default().push(TicketFile {
                name,
                path: inner.path(),
                size,
                is_meta,
            });
        }
        let mut group: Vec<TicketSlugGroup> = by_slug.into_iter().collect();
        for (_, files) in &mut group {
            files.sort_by(|a, b| b.is_meta.cmp(&a.is_meta).then(a.name.cmp(&b.name)));
        }
        years.push((year, group));
    }
    years.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(years)
}

/// Compute `(primary_href, blob_links)` for a ticket group. The
/// primary link is the metadata sidecar when present, else the first
/// blob; remaining files become extra links.
fn ticket_links(year: &str, files: &[TicketFile]) -> (String, Vec<(String, String)>) {
    let primary = files
        .iter()
        .find(|f| f.is_meta)
        .or_else(|| files.first())
        .expect("group_ticket_files never emits empty groups");
    let primary_href = format!("/tickets/{year}/{}", primary.name);
    let blobs = files
        .iter()
        .filter(|f| f.name != primary.name)
        .map(|f| (f.label().to_string(), format!("/tickets/{year}/{}", f.name)))
        .collect();
    (primary_href, blobs)
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

async fn view_ticket(
    State(state): State<Arc<AppState>>,
    UrlPath((year, slug)): UrlPath<(String, String)>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.tickets_dir(), "tickets")?;
    let year_seg = safe_segment(&year)?;
    let slug_seg = safe_segment(&slug)?;
    let meta_path = dir.join(year_seg).join(format!("{slug_seg}.meta.json"));
    let value = read_json(&meta_path).map_err(|e| AppError {
        status: if !meta_path.exists() {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        },
        err: e,
    })?;
    let provider = pick_str(&value, &["provider"]).unwrap_or_else(|| slug.clone());
    let reference = pick_str(&value, &["reservationNumber", "identifier"]).unwrap_or_default();
    let fields = [
        ("Provider", pick_str(&value, &["provider"])),
        (
            "Reference",
            pick_str(&value, &["reservationNumber", "identifier"]),
        ),
        ("File", pick_str(&value, &["file"])),
        ("Content type", pick_str(&value, &["contentType"])),
        (
            "Received",
            pick_str(&value, &["receivedAt"]).map(|s| short_date(&s)),
        ),
    ];
    // Collect every file in the ticket group so the view page links
    // to both the sidecar json and the blob(s) beside it.
    let group = group_ticket_files(dir)?
        .into_iter()
        .find(|(y, _)| y == &year)
        .map(|(_, groups)| groups)
        .unwrap_or_default();
    let files = group
        .into_iter()
        .find(|(s, _)| s == &slug)
        .map(|(_, f)| f)
        .unwrap_or_default();
    let (primary_href_rel, blobs_rel) = ticket_links(&year, &files);
    let primary_href = state.url(&primary_href_rel);
    let blobs: Vec<(String, String)> = blobs_rel
        .into_iter()
        .map(|(label, path)| (label, state.url(&path)))
        .collect();
    Ok(Html(page(
        &state,
        &provider,
        &detail_view(&reference, &fields, &primary_href, &blobs, None),
    )))
}

fn content_type_for(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "json" => "application/json",
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

async fn api_reservations(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let dir = require_dir(state.reservations_dir(), "reservations")?;
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

/// Per-extractor stats table, aggregated on request from
/// `$XDG_STATE_HOME/mailsift/events.ndjson`. The file is written by
/// the milter and (by default) by `imap-scan` and `maildir-scan`.
/// When no log exists yet the page renders an empty panel telling
/// the user where mailsift would write one.
async fn show_stats(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let Some(log_path) = crate::stats::default_log_path() else {
        return Ok(Html(page(
            &state,
            "Stats",
            "<div class=\"empty\">no <code>$XDG_STATE_HOME</code> or <code>$HOME</code> \
             set; can't locate an events log</div>",
        )));
    };
    if !log_path.exists() {
        return Ok(Html(page(
            &state,
            "Stats",
            &format!(
                "<div class=\"empty\">no events recorded yet at \
                 <code>{}</code>. Run <code>mailsift imap-scan</code> or the milter \
                 (or the maildir scanner) at least once to populate it.</div>",
                esc(&log_path.display().to_string())
            ),
        )));
    }
    let stats = crate::stats::aggregate(&log_path)?;
    if stats.is_empty() {
        return Ok(Html(page(
            &state,
            "Stats",
            "<div class=\"empty\">no events recorded</div>",
        )));
    }
    let rows = stats
        .iter()
        .map(|s| {
            let mean = s
                .mean_duration_ms
                .map(|m| format!("{m:.0}"))
                .unwrap_or_else(|| "-".to_string());
            let skipped = s.skipped_headers + s.skipped_body + s.skipped_dkim;
            let last_domain = s.recent_domains.last().map(String::as_str).unwrap_or("-");
            format!(
                "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
                 <td class=\"num\">{}</td><td class=\"num\">{}</td>\
                 <td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td></tr>",
                esc(&s.name),
                s.runs,
                s.produced,
                s.empty,
                s.failed,
                skipped,
                esc(&mean),
                esc(last_domain),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let failures = crate::stats::aggregate_recent_failures(&log_path).unwrap_or_default();
    let failures_section = render_recent_failures(&failures);
    let body = format!(
        "<table><thead><tr>\
         <th>extractor</th>\
         <th class=\"num\">runs</th>\
         <th class=\"num\">produced</th>\
         <th class=\"num\">empty</th>\
         <th class=\"num\">failed</th>\
         <th class=\"num\">skipped</th>\
         <th class=\"num\">mean&nbsp;ms</th>\
         <th>last domain</th>\
         </tr></thead><tbody>{rows}</tbody></table>\
         <p class=\"muted\">Aggregated from <code>{}</code>. \
         Skipped folds headers/body/DKIM prefilter reasons; mean ms is over runs \
         that actually forked the extractor.</p>\
         {failures_section}",
        esc(&log_path.display().to_string()),
    );
    Ok(Html(page(&state, "Stats", &body)))
}

/// Render the most-recent failures as a table, or an empty-state note
/// when there are none. Timestamps are shown as ISO-8601 UTC so a
/// browser without JS still gets a sortable string.
fn render_recent_failures(failures: &[crate::stats::RecentFailure]) -> String {
    if failures.is_empty() {
        return "<h2>Recent failures</h2>\
                <div class=\"empty\">no extractor failures recorded</div>"
            .to_string();
    }
    let rows = failures
        .iter()
        .map(|f| {
            let when = chrono::DateTime::<chrono::Utc>::from_timestamp(f.ts, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M:%SZ").to_string())
                .unwrap_or_else(|| f.ts.to_string());
            let domain = f.from_domain.as_deref().unwrap_or("-");
            let err = f.error.as_deref().unwrap_or("");
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td></tr>",
                esc(&when),
                esc(&f.extractor),
                esc(domain),
                esc(err),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        "<h2>Recent failures</h2>\
         <table><thead><tr>\
         <th>when</th><th>extractor</th><th>from</th><th>error</th>\
         </tr></thead><tbody>{rows}</tbody></table>"
    )
}

async fn api_stats(State(_state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let Some(log_path) = crate::stats::default_log_path() else {
        return Ok(Json(Value::Array(vec![])));
    };
    if !log_path.exists() {
        return Ok(Json(Value::Array(vec![])));
    }
    let stats = crate::stats::aggregate(&log_path)?;
    Ok(Json(serde_json::to_value(stats)?))
}

async fn api_recent_failures(State(_state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let Some(log_path) = crate::stats::default_log_path() else {
        return Ok(Json(Value::Array(vec![])));
    };
    if !log_path.exists() {
        return Ok(Json(Value::Array(vec![])));
    }
    let failures = crate::stats::aggregate_recent_failures(&log_path)?;
    Ok(Json(serde_json::to_value(failures)?))
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

/// Serve `<dir>/<year>/<name>`, picking the response content-type from
/// the filename extension. JSON records and their companion blobs
/// (e.g. `<slug>.pdf` beside `<slug>.json`) live in the same shard and
/// share this handler.
fn serve_shard_file(dir: &Path, year: &str, name: &str) -> Result<Response, AppError> {
    let year = safe_segment(year)?;
    let name = safe_segment(name)?;
    let path = dir.join(year).join(name);
    let body = fs::read(&path).map_err(|e| read_status(&path, e))?;
    let ct = content_type_for(name);
    Ok(([(header::CONTENT_TYPE, HeaderValue::from_static(ct))], body).into_response())
}

/// Read `<dir>/<year>/<slug>.json` as parsed JSON, mapping filesystem
/// errors to appropriate HTTP statuses. Used by the detail-view
/// handlers.
fn read_shard_json(dir: &Path, year: &str, slug: &str) -> Result<Value, AppError> {
    let year = safe_segment(year)?;
    let slug = safe_segment(slug)?;
    let path = dir.join(year).join(format!("{slug}.json"));
    let body = fs::read_to_string(&path).map_err(|e| read_status(&path, e))?;
    serde_json::from_str(&body).map_err(|e| AppError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        err: anyhow::Error::from(e).context(format!("parsing {}", path.display())),
    })
}

/// Read `<dir>/<name>` (flat, no year shard) as parsed JSON.
fn read_flat_json(dir: &Path, name: &str) -> Result<Value, AppError> {
    let name = safe_segment(name)?;
    let path = dir.join(name);
    let body = fs::read_to_string(&path).map_err(|e| read_status(&path, e))?;
    serde_json::from_str(&body).map_err(|e| AppError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        err: anyhow::Error::from(e).context(format!("parsing {}", path.display())),
    })
}

/// Render a detail-view page body: an optional subtitle, a definition
/// list of non-empty fields, and a footer with `raw json` / blob /
/// vendor links. Fields with an empty or `None` value are silently
/// skipped so callers can pass through the union of keys a kind might
/// have.
fn detail_view(
    subtitle: &str,
    fields: &[(&str, Option<String>)],
    json_href: &str,
    blobs: &[(String, String)],
    vendor: Option<&str>,
) -> String {
    let mut out = String::new();
    if !subtitle.is_empty() {
        out.push_str(&format!("<p class=\"muted\">{}</p>", esc(subtitle)));
    }
    out.push_str("<dl class=\"detail\">");
    for (label, value) in fields {
        let Some(v) = value else { continue };
        if v.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("<dt>{}</dt><dd>{}</dd>", esc(label), esc(v)));
    }
    out.push_str("</dl>");
    // The list-page `links_cell` renders the JSON link as plain `json`
    // because the row title already implies "the record". On a detail
    // view the reader is looking *at* the record, so make the label
    // explicit that this link takes you to the raw source.
    out.push_str("<p class=\"links\"><a href=\"");
    out.push_str(&esc(json_href));
    out.push_str("\">raw json</a>");
    for (label, href) in blobs {
        out.push_str(&format!(
            " &middot; <a href=\"{}\">{}</a>",
            esc(href),
            esc(label),
        ));
    }
    if let Some(url) = vendor {
        out.push_str(&format!(
            " &middot; <a href=\"{}\" rel=\"noopener noreferrer\">open</a>",
            esc(url),
        ));
    }
    out.push_str("</p>");
    out
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

/// Extract the vendor / detail URL for an artifact if it carries one.
///
/// Looks at a handful of well-known keys (`url`, `orderUrl`, ...) and
/// requires the value to be an absolute `http(s)://` URL to avoid
/// rendering unclickable strings or opening a "javascript:" link.
///
/// Also picks up an `invoice.url` sub-object (used by some bill
/// extractors) or `url` inside `potentialAction` (schema.org's Action
/// pattern for "Track this parcel").
fn vendor_url(value: &Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "url",
        "orderUrl",
        "paymentUrl",
        "trackingUrl",
        "managementUrl",
        "pdfLink",
    ];
    if let Some(u) = pick_str(value, KEYS).filter(|u| is_safe_http_url(u)) {
        return Some(u);
    }
    if let Some(inv) = value.get("invoice")
        && let Some(u) = pick_str(inv, &["url", "pdfLink"]).filter(|u| is_safe_http_url(u))
    {
        return Some(u);
    }
    if let Some(action) = value.get("potentialAction")
        && let Some(u) = pick_str(action, &["url", "target"]).filter(|u| is_safe_http_url(u))
    {
        return Some(u);
    }
    None
}

fn is_safe_http_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://")
}

/// Render a "links" cell for a list row: always a link to the raw
/// JSON, then optional companion-blob links (e.g. a sibling `.pdf`),
/// then an optional "open" link to the artifact's vendor URL. `open`
/// links carry `rel=\"noopener noreferrer\"` since they leave our
/// origin.
fn links_cell(json_href: &str, blobs: &[(String, String)], vendor: Option<&str>) -> String {
    let mut out = format!("<a href=\"{}\">json</a>", esc(json_href));
    for (label, href) in blobs {
        out.push_str(&format!(
            " &middot; <a href=\"{}\">{}</a>",
            esc(href),
            esc(label),
        ));
    }
    if let Some(url) = vendor {
        out.push_str(&format!(
            " &middot; <a href=\"{}\" rel=\"noopener noreferrer\">open</a>",
            esc(url),
        ));
    }
    out
}

/// Companion blobs that live beside a `<year>/<slug>.json` artifact.
/// Returns `(label, filename)` pairs sorted for stable rendering. The
/// label is the lowercased extension (`"pdf"`, `"pkpass"`, ...).
fn sibling_blobs(dir: &Path, year: &str, slug: &str) -> Vec<(String, String)> {
    let year_dir = dir.join(year);
    let mut out: Vec<(String, String)> = Vec::new();
    let Ok(entries) = fs::read_dir(&year_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if stem != slug || ext.eq_ignore_ascii_case("json") {
            continue;
        }
        out.push((ext.to_ascii_lowercase(), name));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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
        let reservations = tmp.path().join("reservations/2026");
        fs::create_dir_all(&reservations).unwrap();
        fs::write(
            reservations.join("fixture-air-FX7QT2.json"),
            br#"{"@type":"FlightReservation","reservationNumber":"FX7QT2",
                 "underName":{"name":"J Vernooij"},
                 "reservationFor":{"airline":{"iataCode":"FX","name":"Fixture Air"},
                                   "departureTime":"2026-04-10T08:00:00Z"}}"#,
        )
        .unwrap();
        let config = Config {
            events_dir: Some(events),
            bills_dir: Some(tmp.path().join("bills")),
            parcels_dir: Some(parcels),
            reservations_dir: Some(tmp.path().join("reservations")),
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

    #[test]
    fn parse_any_date_accepts_floating_and_offset_times() {
        // No trailing `Z`: the offset search must not cut the date's
        // own hyphens.
        assert_eq!(
            parse_any_date("2026-04-10T15:00:00"),
            NaiveDate::from_ymd_opt(2026, 4, 10)
        );
        assert_eq!(
            parse_any_date("2026-04-10T15:00:00+02:00"),
            NaiveDate::from_ymd_opt(2026, 4, 10)
        );
        assert_eq!(
            parse_any_date("2026-04-10T15:00:00-05:00"),
            NaiveDate::from_ymd_opt(2026, 4, 10)
        );
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

    #[test]
    fn vendor_url_prefers_url_key() {
        let v: Value =
            serde_json::from_str(r#"{"url":"https://vendor.example/x", "other":"nope"}"#).unwrap();
        assert_eq!(vendor_url(&v).as_deref(), Some("https://vendor.example/x"));
    }

    #[test]
    fn vendor_url_falls_back_to_kind_specific_keys() {
        let v: Value =
            serde_json::from_str(r#"{"trackingUrl":"https://carrier.example/t/1"}"#).unwrap();
        assert_eq!(
            vendor_url(&v).as_deref(),
            Some("https://carrier.example/t/1")
        );
        let v: Value =
            serde_json::from_str(r#"{"managementUrl":"https://sub.example/manage"}"#).unwrap();
        assert_eq!(
            vendor_url(&v).as_deref(),
            Some("https://sub.example/manage")
        );
    }

    #[test]
    fn vendor_url_reads_nested_invoice_url() {
        let v: Value =
            serde_json::from_str(r#"{"invoice":{"url":"https://vendor.example/inv.pdf"}}"#)
                .unwrap();
        assert_eq!(
            vendor_url(&v).as_deref(),
            Some("https://vendor.example/inv.pdf")
        );
    }

    #[test]
    fn vendor_url_rejects_non_http() {
        let v: Value = serde_json::from_str(r#"{"url":"javascript:alert(1)"}"#).unwrap();
        assert_eq!(vendor_url(&v), None);
        let v: Value = serde_json::from_str(r#"{"url":"file:///etc/passwd"}"#).unwrap();
        assert_eq!(vendor_url(&v), None);
        let v: Value = serde_json::from_str(r#"{"url":"just-a-string"}"#).unwrap();
        assert_eq!(vendor_url(&v), None);
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

    #[test]
    fn ticket_slug_strips_meta_json_suffix() {
        assert_eq!(
            ticket_slug("boarding-pass.meta.json"),
            Some(("boarding-pass".to_string(), true))
        );
    }

    #[test]
    fn ticket_slug_strips_single_extension_for_blobs() {
        assert_eq!(
            ticket_slug("boarding-pass.pdf"),
            Some(("boarding-pass".to_string(), false))
        );
        assert_eq!(
            ticket_slug("pass.pkpass"),
            Some(("pass".to_string(), false))
        );
    }

    #[test]
    fn ticket_slug_declines_extensionless_names() {
        assert_eq!(ticket_slug("no-extension"), None);
    }

    #[tokio::test]
    async fn ticket_row_groups_meta_json_and_blob() {
        // A ticket blob (`.pdf`) and its `.meta.json` sidecar share a
        // slug and should render as one row with both download links.
        let tmp = tempfile::tempdir().unwrap();
        let year = tmp.path().join("tickets/2026");
        fs::create_dir_all(&year).unwrap();
        fs::write(year.join("easyjet-ezy2521.pdf"), b"%PDF-1.4\n").unwrap();
        fs::write(
            year.join("easyjet-ezy2521.meta.json"),
            br#"{"slug":"easyjet-ezy2521","file":"easyjet-ezy2521.pdf"}"#,
        )
        .unwrap();
        let config = Config {
            tickets_dir: Some(tmp.path().join("tickets")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/tickets").await;
        assert_eq!(status, StatusCode::OK);
        let tbody = body
            .split("<tbody>")
            .nth(1)
            .and_then(|s| s.split("</tbody>").next())
            .expect("no tbody in response");
        // Exactly one row per ticket group.
        assert_eq!(
            tbody.matches("<tr>").count(),
            1,
            "expected 1 body row: {tbody}"
        );
        // Both links present on that row.
        assert!(
            body.contains("/tickets/2026/easyjet-ezy2521.meta.json"),
            "meta.json link missing: {body}"
        );
        assert!(
            body.contains("/tickets/2026/easyjet-ezy2521.pdf"),
            "pdf link missing: {body}"
        );
    }

    #[tokio::test]
    async fn bill_row_shows_sibling_pdf_link() {
        let (tmp, mut config) = fixture();
        // Fixture already wrote `bills/2026/acme-INV1.json`; add a
        // sibling PDF blob and check the row surfaces it.
        fs::write(tmp.path().join("bills/2026/acme-INV1.pdf"), b"%PDF-1.4\n").unwrap();
        config.bills_dir = Some(tmp.path().join("bills"));
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/bills").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("/bills/2026/acme-INV1.json"),
            "json link missing: {body}"
        );
        assert!(
            body.contains("/bills/2026/acme-INV1.pdf"),
            "pdf link missing: {body}"
        );
        // The row should still be one row.
        let tbody = body
            .split("<tbody>")
            .nth(1)
            .and_then(|s| s.split("</tbody>").next())
            .unwrap_or("");
        assert_eq!(tbody.matches("<tr>").count(), 1, "expected 1 row: {tbody}");
    }

    #[tokio::test]
    async fn bill_pdf_served_as_pdf_content_type() {
        let (tmp, mut config) = fixture();
        fs::write(tmp.path().join("bills/2026/acme-INV1.pdf"), b"%PDF-1.4\n").unwrap();
        config.bills_dir = Some(tmp.path().join("bills"));
        let app = router(state_with(config, ""));
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/bills/2026/acme-INV1.pdf")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/pdf")
        );
    }

    #[tokio::test]
    async fn bills_sorted_by_date_desc_across_years() {
        // Two bills: one dated 2025-01, one 2026-03. Newer must render
        // first regardless of the alphabetical order of slugs / shard
        // year they live in.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("bills/2025")).unwrap();
        fs::create_dir_all(tmp.path().join("bills/2026")).unwrap();
        fs::write(
            tmp.path().join("bills/2025/zzz.json"),
            br#"{"payee":"Old","invoiceNumber":"OLD","dueDate":"2025-01-15"}"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("bills/2026/aaa.json"),
            br#"{"payee":"New","invoiceNumber":"NEW","dueDate":"2026-03-15"}"#,
        )
        .unwrap();
        let config = Config {
            bills_dir: Some(tmp.path().join("bills")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/bills").await;
        let new_pos = body.find("NEW").expect("NEW missing");
        let old_pos = body.find("OLD").expect("OLD missing");
        assert!(
            new_pos < old_pos,
            "expected NEW (2026-03) before OLD (2025-01): {body}"
        );
    }

    #[tokio::test]
    async fn parcels_sorted_by_date_desc_not_tracking_number() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("parcels")).unwrap();
        // Alphabetically A < Z; date-wise Z is newer. Z must sort first.
        fs::write(
            tmp.path().join("parcels/A.json"),
            br#"{"trackingNumber":"A","deliveryStatus":"OrderDelivered",
                 "receivedAt":"2021-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("parcels/Z.json"),
            br#"{"trackingNumber":"Z","deliveryStatus":"OutForDelivery",
                 "receivedAt":"2026-08-01T00:00:00Z"}"#,
        )
        .unwrap();
        let config = Config {
            parcels_dir: Some(tmp.path().join("parcels")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/parcels").await;
        let tbody = body
            .split("<tbody>")
            .nth(1)
            .and_then(|s| s.split("</tbody>").next())
            .expect("no tbody");
        let z_pos = tbody.find(">Z<").expect("Z missing");
        let a_pos = tbody.find(">A<").expect("A missing");
        assert!(z_pos < a_pos, "expected Z (newer) before A: {tbody}");
    }

    #[tokio::test]
    async fn parcels_carrier_uses_name_not_id() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("parcels")).unwrap();
        fs::write(
            tmp.path().join("parcels/T.json"),
            br#"{"trackingNumber":"T","deliveryStatus":"OrderDelivered",
                 "provider":{"@id":"amazon-uk","name":"Amazon"}}"#,
        )
        .unwrap();
        let config = Config {
            parcels_dir: Some(tmp.path().join("parcels")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/parcels").await;
        assert!(body.contains(">Amazon<"), "carrier name missing: {body}");
        assert!(!body.contains(">amazon-uk<"), "carrier id leaked: {body}");
    }

    #[tokio::test]
    async fn reservation_date_column_is_short_date() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/reservations").await;
        assert!(
            body.contains(">2026-04-10<"),
            "reservation date not shortened: {body}"
        );
        assert!(
            !body.contains(">2026-04-10T08:00:00Z<"),
            "full ISO datetime still rendered: {body}"
        );
    }

    #[tokio::test]
    async fn bill_list_pager_limits_rows() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("bills/2026")).unwrap();
        for i in 0..(PAGE_SIZE + 5) {
            fs::write(
                tmp.path().join(format!("bills/2026/bill-{i:03}.json")),
                format!(
                    r#"{{"payee":"P","invoiceNumber":"INV{i:03}","dueDate":"2026-01-{:02}"}}"#,
                    (i % 28) + 1
                ),
            )
            .unwrap();
        }
        let config = Config {
            bills_dir: Some(tmp.path().join("bills")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/bills").await;
        let tbody = body
            .split("<tbody>")
            .nth(1)
            .and_then(|s| s.split("</tbody>").next())
            .expect("no tbody");
        assert_eq!(
            tbody.matches("<tr>").count(),
            PAGE_SIZE,
            "first page should show PAGE_SIZE rows: {tbody}"
        );
        assert!(body.contains("page 1 of 2"), "pager missing: {body}");
    }

    #[tokio::test]
    async fn subscriptions_price_shows_currency_and_free() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("subscriptions")).unwrap();
        fs::write(
            tmp.path().join("subscriptions/paid.json"),
            br#"{"name":"Paid","price":1.59,"priceCurrency":"GBP",
                 "orderDate":"2026-01-01"}"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("subscriptions/free.json"),
            br#"{"name":"Free","price":0.0,"priceCurrency":"USD",
                 "orderDate":"2026-01-02"}"#,
        )
        .unwrap();
        let config = Config {
            subscriptions_dir: Some(tmp.path().join("subscriptions")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/subscriptions").await;
        assert!(
            body.contains("1.59 GBP"),
            "paid row missing currency: {body}"
        );
        assert!(
            body.contains(">free<"),
            "free row not labelled 'free': {body}"
        );
    }

    #[test]
    fn format_price_free_zero() {
        let v: Value = serde_json::from_str(r#"{"price":0.0,"priceCurrency":"GBP"}"#).unwrap();
        assert_eq!(format_price(&v), "free");
    }

    #[test]
    fn format_price_with_currency() {
        let v: Value = serde_json::from_str(r#"{"price":9.99,"priceCurrency":"EUR"}"#).unwrap();
        assert_eq!(format_price(&v), "9.99 EUR");
    }

    #[test]
    fn format_price_missing() {
        let v: Value = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(format_price(&v), "");
    }

    #[tokio::test]
    async fn reservation_route_flight() {
        let v: Value = serde_json::from_str(
            r#"{"reservationFor":{"@type":"Flight",
                "departureAirport":{"iataCode":"LHR"},
                "arrivalAirport":{"iataCode":"AMS"}}}"#,
        )
        .unwrap();
        assert_eq!(reservation_route(&v).as_deref(), Some("LHR -> AMS"));
    }

    #[tokio::test]
    async fn reservation_route_train_uses_station_names() {
        let v: Value = serde_json::from_str(
            r#"{"reservationFor":{"@type":"TrainTrip",
                "departureStation":{"name":"London St Pancras"},
                "arrivalStation":{"name":"Amsterdam Centraal"}}}"#,
        )
        .unwrap();
        assert_eq!(
            reservation_route(&v).as_deref(),
            Some("London St Pancras -> Amsterdam Centraal")
        );
    }

    #[tokio::test]
    async fn reservation_route_hotel_none() {
        let v: Value =
            serde_json::from_str(r#"{"reservationFor":{"@type":"LodgingBusiness","name":"ibis"}}"#)
                .unwrap();
        assert!(reservation_route(&v).is_none());
    }

    #[tokio::test]
    async fn not_found_navbar_has_configured_kinds() {
        // AppError paths used to render 404 with a default (empty)
        // AppState, dropping every kind link. The rerender middleware
        // now swaps in the request's state so the nav stays intact.
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/bills/2026/does-not-exist.json").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body.contains(">bills</a>"),
            "bills link missing from 404 nav: {body}"
        );
    }

    #[tokio::test]
    async fn page_query_falls_back_on_garbage() {
        // A shared URL with `?page=abc` should not 400; treat it as
        // page 1 instead.
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, _) = get(&app, "/bills?page=abc").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn subscription_active_recent_receivedat() {
        let today = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let received = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert!(is_subscription_active(Some(received), Some("P1M"), today));
    }

    #[tokio::test]
    async fn subscription_inactive_when_stale() {
        // 2*P1M = 60 days; a 6-month-old record is definitely stale.
        let today = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let received = NaiveDate::from_ymd_opt(2025, 12, 1).unwrap();
        assert!(!is_subscription_active(Some(received), Some("P1M"), today));
    }

    #[test]
    fn parse_iso_duration_common_forms() {
        assert_eq!(parse_iso_duration_days("P1M"), Some(30));
        assert_eq!(parse_iso_duration_days("P1Y"), Some(365));
        assert_eq!(parse_iso_duration_days("P7D"), Some(7));
        assert_eq!(parse_iso_duration_days("PT1H"), None); // time part unsupported
    }

    #[tokio::test]
    async fn bill_view_page_renders_field_list() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/bills/2026/acme-INV1/view").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<dt>Payee</dt><dd>Acme</dd>"), "body: {body}");
        assert!(
            body.contains("<dt>Invoice number</dt><dd>INV1</dd>"),
            "body: {body}"
        );
        assert!(
            body.contains("/bills/2026/acme-INV1.json"),
            "raw json link missing: {body}"
        );
    }

    #[tokio::test]
    async fn bill_list_title_links_to_view() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/bills").await;
        assert!(
            body.contains("/bills/2026/acme-INV1/view"),
            "list should link to view page: {body}"
        );
    }

    #[tokio::test]
    async fn ticket_view_renders_provider_and_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let year = tmp.path().join("tickets/2026");
        fs::create_dir_all(&year).unwrap();
        fs::write(year.join("klm-abc.pdf"), b"%PDF-1.4\n").unwrap();
        fs::write(
            year.join("klm-abc.meta.json"),
            br#"{"slug":"klm-abc","file":"klm-abc.pdf","provider":"KLM",
                 "reservationNumber":"ABC123","receivedAt":"2026-06-01T00:00:00Z"}"#,
        )
        .unwrap();
        let config = Config {
            tickets_dir: Some(tmp.path().join("tickets")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/tickets/2026/klm-abc/view").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<h1>KLM</h1>"), "body: {body}");
        assert!(
            body.contains("<dt>Reference</dt><dd>ABC123</dd>"),
            "body: {body}"
        );
        assert!(
            body.contains("/tickets/2026/klm-abc.pdf"),
            "pdf link missing: {body}"
        );
    }

    #[tokio::test]
    async fn parcel_delivered_row_falls_back_to_received_at() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("parcels")).unwrap();
        // Delivered parcel with no actualDeliveryTime; row date should
        // fall back to receivedAt so the column doesn't render blank.
        fs::write(
            tmp.path().join("parcels/DEL.json"),
            br#"{"trackingNumber":"DEL","deliveryStatus":"OrderDelivered",
                 "receivedAt":"2026-05-10T00:00:00Z"}"#,
        )
        .unwrap();
        let config = Config {
            parcels_dir: Some(tmp.path().join("parcels")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/parcels").await;
        assert!(
            body.contains(">2026-05-10<"),
            "expected receivedAt fallback in date column: {body}"
        );
    }

    #[tokio::test]
    async fn bills_year_chip_filters_shard() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("bills/2025")).unwrap();
        fs::create_dir_all(tmp.path().join("bills/2026")).unwrap();
        fs::write(
            tmp.path().join("bills/2025/old.json"),
            br#"{"payee":"OldCo","invoiceNumber":"O1","dueDate":"2025-01-15"}"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("bills/2026/new.json"),
            br#"{"payee":"NewCo","invoiceNumber":"N1","dueDate":"2026-03-15"}"#,
        )
        .unwrap();
        let config = Config {
            bills_dir: Some(tmp.path().join("bills")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/bills?year=2025").await;
        assert!(body.contains("OldCo"), "2025 bill missing: {body}");
        assert!(
            !body.contains("NewCo"),
            "2026 bill should be filtered out: {body}"
        );
        // Year chip for 2026 should still be rendered so the user
        // can jump there.
        assert!(
            body.contains(">2026</a>") || body.contains("year=2026"),
            "2026 chip missing: {body}"
        );
    }

    #[tokio::test]
    async fn bills_search_filters_rows() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("bills/2026")).unwrap();
        fs::write(
            tmp.path().join("bills/2026/a.json"),
            br#"{"payee":"Acme","invoiceNumber":"A","dueDate":"2026-01-01"}"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("bills/2026/b.json"),
            br#"{"payee":"Zenith","invoiceNumber":"Z","dueDate":"2026-02-01"}"#,
        )
        .unwrap();
        let config = Config {
            bills_dir: Some(tmp.path().join("bills")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/bills?q=acme").await;
        assert!(body.contains("Acme"), "Acme missing: {body}");
        assert!(
            !body.contains("Zenith"),
            "Zenith should be filtered out: {body}"
        );
    }

    #[test]
    fn list_query_to_query_string_preserves_filters() {
        let q = ListQuery {
            page: Some(3),
            q: Some("hello world".into()),
            year: Some("2026".into()),
        };
        let s = q.to_query_string(None);
        assert!(s.starts_with('?'));
        assert!(s.contains("q=hello+world"));
        assert!(s.contains("year=2026"));
        assert!(s.contains("page=3"));
    }

    #[test]
    fn list_query_page_one_omitted() {
        // page=1 is redundant with the default; keep URLs tidy.
        let q = ListQuery {
            page: Some(1),
            q: None,
            year: None,
        };
        assert_eq!(q.to_query_string(None), "");
    }

    #[tokio::test]
    async fn subscription_view_labels_json_link_as_raw() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("subs")).unwrap();
        fs::write(
            tmp.path().join("subs/x.json"),
            br#"{"name":"X","price":1.0,"priceCurrency":"GBP",
                 "orderDate":"2026-01-01"}"#,
        )
        .unwrap();
        let config = Config {
            subscriptions_dir: Some(tmp.path().join("subs")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/subscriptions/x/view").await;
        assert!(
            body.contains(">raw json</a>"),
            "view page should label the json link 'raw json': {body}"
        );
    }

    #[tokio::test]
    async fn overview_subscription_card_shows_active_count() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("subs")).unwrap();
        // Recent = active.
        let now = Utc::now();
        let recent_iso = now.to_rfc3339();
        // Long-ago = inactive.
        let ancient_iso = "2013-06-17T00:00:00Z";
        fs::write(
            tmp.path().join("subs/active.json"),
            format!(
                r#"{{"name":"Active","price":1,"priceCurrency":"GBP",
                     "receivedAt":"{recent_iso}","subscriptionDuration":"P1M"}}"#
            ),
        )
        .unwrap();
        fs::write(
            tmp.path().join("subs/stale.json"),
            format!(
                r#"{{"name":"Stale","price":1,"priceCurrency":"GBP",
                     "receivedAt":"{ancient_iso}","subscriptionDuration":"P1M"}}"#
            ),
        )
        .unwrap();
        let config = Config {
            subscriptions_dir: Some(tmp.path().join("subs")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/").await;
        // Card should carry a "1 active" subtitle beside the total 2.
        assert!(
            body.contains("1 active"),
            "expected '1 active' subtitle on card: {body}"
        );
    }

    #[tokio::test]
    async fn all_page_paginates_recent() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("bills/2026")).unwrap();
        for i in 0..(PAGE_SIZE + 5) {
            fs::write(
                tmp.path().join(format!("bills/2026/bill-{i:03}.json")),
                format!(
                    r#"{{"payee":"P{i:03}","invoiceNumber":"I{i:03}","dueDate":"2025-01-{:02}"}}"#,
                    (i % 28) + 1
                ),
            )
            .unwrap();
        }
        let config = Config {
            bills_dir: Some(tmp.path().join("bills")),
            ..Config::default()
        };
        let app = router(state_with(config, ""));
        let (_, body) = get(&app, "/all").await;
        assert!(body.contains("page 1 of 2"), "pager missing: {body}");
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
        assert!(body.contains(">reservations<"));
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

    #[tokio::test]
    async fn reservations_list_shows_row() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/reservations").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Fixture Air"), "provider missing: {body}");
        assert!(body.contains("FX7QT2"), "reference missing: {body}");
        assert!(body.contains("J Vernooij"), "under name missing: {body}");
    }

    #[tokio::test]
    async fn reservations_overview_card_counts() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        let card = body
            .split("<div class=\"card\">")
            .skip(1)
            .find(|c| c.contains("href=\"/reservations\""))
            .expect("no reservations card");
        assert!(
            card.contains("<div class=\"n\">1</div>"),
            "expected a count of 1; card: {card}"
        );
    }

    #[tokio::test]
    async fn reservations_appear_in_feed() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/all").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<span class=\"badge reservation\">reservation</span>"),
            "no reservation badge in feed: {body}"
        );
        assert!(body.contains("Fixture Air"), "feed row missing: {body}");
    }

    #[tokio::test]
    async fn reservation_download_serves_json() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/reservations/2026/fixture-air-FX7QT2.json").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["reservationNumber"], "FX7QT2");
    }

    #[tokio::test]
    async fn reservations_json_api() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/api/reservations.json").await;
        assert_eq!(status, StatusCode::OK);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["reservationNumber"], "FX7QT2");
        assert_eq!(parsed[0]["_year"], "2026");
        assert_eq!(parsed[0]["_slug"], "fixture-air-FX7QT2");
    }

    #[tokio::test]
    async fn reservations_hidden_when_unconfigured() {
        let (_tmp, mut config) = fixture();
        config.reservations_dir = None;
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains(">reservations<"),
            "reservations should be hidden"
        );
    }

    #[tokio::test]
    async fn reservations_unconfigured_returns_404() {
        let (_tmp, mut config) = fixture();
        config.reservations_dir = None;
        let app = router(state_with(config, ""));
        let (status, _) = get(&app, "/reservations").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn reservation_provider_prefers_airline() {
        let v: Value = serde_json::from_str(
            r#"{"provider":{"name":"Agency"},
                "reservationFor":{"airline":{"name":"Fixture Air"}}}"#,
        )
        .unwrap();
        assert_eq!(reservation_provider(&v).as_deref(), Some("Fixture Air"));
    }

    #[test]
    fn reservation_provider_falls_back_to_venue_name() {
        let v: Value =
            serde_json::from_str(r#"{"reservationFor":{"name":"Fixture Inn"}}"#).unwrap();
        assert_eq!(reservation_provider(&v).as_deref(), Some("Fixture Inn"));
    }

    #[test]
    fn reservation_provider_accepts_bare_string_node() {
        let v: Value = serde_json::from_str(r#"{"broker":"Fixture Travel"}"#).unwrap();
        assert_eq!(reservation_provider(&v).as_deref(), Some("Fixture Travel"));
    }

    #[test]
    fn reservation_date_prefers_trip_date_over_received_at() {
        let v: Value = serde_json::from_str(
            r#"{"receivedAt":"2026-01-01T00:00:00Z",
                "reservationFor":{"departureTime":"2026-04-10T08:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(
            reservation_date(&v).and_then(|d| parse_any_date(&d)),
            NaiveDate::from_ymd_opt(2026, 4, 10)
        );
    }

    #[test]
    fn reservation_date_uses_checkin_for_lodging() {
        let v: Value = serde_json::from_str(
            r#"{"checkinTime":"2026-04-10T15:00:00",
                "reservationFor":{"name":"Fixture Inn"}}"#,
        )
        .unwrap();
        assert_eq!(
            reservation_date(&v).and_then(|d| parse_any_date(&d)),
            NaiveDate::from_ymd_opt(2026, 4, 10)
        );
    }

    #[test]
    fn reservation_number_falls_back_to_identifier() {
        let v: Value = serde_json::from_str(r#"{"identifier":"ID-9"}"#).unwrap();
        assert_eq!(reservation_number(&v).as_deref(), Some("ID-9"));
    }

    /// The /stats page is always in the nav, even for a config with
    /// no artifact directories. The empty-state message is rendered
    /// when the log file doesn't exist yet.
    #[tokio::test]
    async fn stats_page_renders_empty_state_when_no_log() {
        // Point XDG_STATE_HOME at a directory that exists but has no
        // events log. The env-mutation dance mirrors the pattern in
        // `stats::tests::default_log_path_uses_xdg_state_home`.
        let tmp = tempfile::tempdir().unwrap();
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
            std::env::remove_var("HOME");
        }

        let (_fx_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/stats").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("no events recorded yet"), "body: {body}");
        assert!(body.contains("mailsift/events.ndjson"), "body: {body}");

        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v);
            }
        }
    }

    #[tokio::test]
    async fn nav_includes_stats_link() {
        let (_tmp, config) = fixture();
        let app = router(state_with(config, ""));
        let (status, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(">stats</a>"), "no stats link in nav: {body}");
    }
}
