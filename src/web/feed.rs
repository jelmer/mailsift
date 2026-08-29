//! The merged artifact feed behind the homepage and `/all`: one
//! date-sorted row per artifact, whatever its kind.

use std::fs;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::response::Html;
use chrono::{NaiveDate, Utc};

use super::AppState;
use super::error::AppError;
use super::render::{esc, links_cell, page};
use super::scan::{
    count_flat, count_year, ics_field, mtime_date, mtime_date_opt, parse_any_date, pick_str,
    read_dir_or_empty, vendor_url, walk_flat_json, walk_year_json,
};

/// Cap on the number of upcoming/recent items on the homepage. Above
/// this, the "view all" link takes over.
const FEED_HOMEPAGE_LIMIT: usize = 20;

pub(super) async fn index(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
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
             <td>{links}</td></tr>",
            date = esc(&item.date.to_string()),
            kind = esc(item.kind),
            title = esc(&item.title),
            subtitle = esc(&item.subtitle),
            links = links_cell(&state.url(&item.href), item.vendor_url.as_deref()),
        ));
    }
    format!(
        "<h2>{title}</h2>\
         <table><thead><tr><th>date</th><th>type</th><th></th><th></th><th>links</th></tr></thead>\
         <tbody>{rows}</tbody></table>",
        title = esc(title),
    )
}

pub(super) async fn list_all(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
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
    /// Vendor / detail URL from the artifact JSON, if present.
    vendor_url: Option<String>,
    /// Root-relative URL to the detail page; run through `state.url`
    /// before rendering.
    href: String,
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
            items.push(FeedItem {
                date,
                kind: "bill",
                title: payee,
                subtitle: invoice,
                vendor_url: vendor_url(&value),
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
                    "receivedAt",
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
                vendor_url: vendor_url(&value),
                href: format!("/parcels/{name}"),
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
            items.push(FeedItem {
                date,
                kind: "receipt",
                title: merchant,
                subtitle: order,
                vendor_url: vendor_url(&value),
                href: format!("/receipts/{year}/{slug}.json"),
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
                    vendor_url: None,
                    href: format!("/tickets/{year}/{name}"),
                });
            }
        }
    }

    Ok(items)
}
