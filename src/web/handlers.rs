//! Per-kind routes: an HTML list view, a raw-file download, and (for
//! the JSON-backed kinds) an `/api/*.json` dump.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::Value;

use super::AppState;
use super::error::{AppError, read_status, require_dir, safe_segment};
use super::render::{esc, human_size, links_cell, page};
use super::scan::{
    ics_field, pick_str, read_dir_or_empty, vendor_url, walk_flat_json, walk_year_json,
};

pub(super) async fn list_events(
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, AppError> {
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

pub(super) async fn get_event(
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

pub(super) async fn list_bills(
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, AppError> {
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
        let vendor = vendor_url(&value);
        let cells = format!(
            "<td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&year),
            esc(&payee.unwrap_or_default()),
            esc(&invoice.unwrap_or_default()),
            esc(&due.unwrap_or_default()),
            esc(&amount),
            links_cell(&href, vendor.as_deref()),
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

pub(super) async fn get_bill(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.bills_dir(), "bills")?;
    serve_json_file(dir, &year, &name)
}

pub(super) async fn list_parcels(
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, AppError> {
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
        let vendor = vendor_url(&value);
        let cells = format!(
            "<td>{}</td><td><span class=\"badge\">{}</span></td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&tracking),
            esc(&carrier),
            esc(&status),
            esc(&eta),
            links_cell(&state.url(&format!("/parcels/{name}")), vendor.as_deref()),
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

pub(super) async fn get_parcel(
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

pub(super) async fn list_receipts(
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for (year, slug, value) in walk_year_json(dir)? {
        let merchant = pick_str(&value, &["merchant", "seller"]).unwrap_or_default();
        let order = pick_str(&value, &["orderNumber", "identifier"]).unwrap_or_default();
        let date = pick_str(&value, &["orderDate", "date"]).unwrap_or_default();
        let vendor = vendor_url(&value);
        let cells = format!(
            "<td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&year),
            esc(&merchant),
            esc(&order),
            esc(&date),
            links_cell(
                &state.url(&format!("/receipts/{year}/{slug}.json")),
                vendor.as_deref(),
            ),
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

pub(super) async fn get_receipt(
    State(state): State<Arc<AppState>>,
    UrlPath((year, name)): UrlPath<(String, String)>,
) -> Result<Response, AppError> {
    let dir = require_dir(state.receipts_dir(), "receipts")?;
    serve_json_file(dir, &year, &name)
}

pub(super) async fn list_subscriptions(
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, AppError> {
    let dir = require_dir(state.subscriptions_dir(), "subscriptions")?;
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, value) in walk_flat_json(dir)? {
        let display = pick_str(&value, &["name", "provider"]).unwrap_or_default();
        let renewal = pick_str(&value, &["renewalDate", "nextPaymentDate"]).unwrap_or_default();
        let price = value
            .get("price")
            .and_then(|v| v.as_str().map(String::from).or_else(|| Some(v.to_string())))
            .unwrap_or_default();
        let vendor = vendor_url(&value);
        let cells = format!(
            "<td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            esc(&display),
            esc(&renewal),
            esc(&price),
            links_cell(
                &state.url(&format!("/subscriptions/{name}")),
                vendor.as_deref(),
            ),
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

pub(super) async fn get_subscription(
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

pub(super) async fn list_tickets(
    State(state): State<Arc<AppState>>,
) -> Result<Html<String>, AppError> {
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

pub(super) async fn get_ticket(
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

pub(super) async fn api_bills(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
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

pub(super) async fn api_parcels(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let dir = require_dir(state.parcels_dir(), "parcels")?;
    let items: Vec<Value> = walk_flat_json(dir)?.into_iter().map(|(_, v)| v).collect();
    Ok(Json(Value::Array(items)))
}

pub(super) async fn api_receipts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
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

pub(super) async fn api_subscriptions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let dir = require_dir(state.subscriptions_dir(), "subscriptions")?;
    let items: Vec<Value> = walk_flat_json(dir)?.into_iter().map(|(_, v)| v).collect();
    Ok(Json(Value::Array(items)))
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
