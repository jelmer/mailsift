//! Reading the artifact directories: directory walks, JSON parsing,
//! and the field-picking helpers that turn a raw artifact document
//! into the handful of values the UI shows.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;

/// Try a series of ISO-ish date formats. Accepts YYYY-MM-DD,
/// YYYY-MM-DDTHH:MM:SS(Z|+00:00), and iCal `YYYYMMDDTHHMMSSZ`.
pub(super) fn parse_any_date(raw: &str) -> Option<NaiveDate> {
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

/// File mtime as a naive UTC date. On any error (missing file, no
/// mtime, out-of-range) fall back to today so the item still surfaces
/// in the feed.
pub(super) fn mtime_date(path: &Path) -> NaiveDate {
    mtime_date_opt(path).unwrap_or_else(|| Utc::now().date_naive())
}

pub(super) fn mtime_date_opt(path: &Path) -> Option<NaiveDate> {
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
pub(super) fn count_flat(dir: &Path, ext: &str) -> Result<usize> {
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
pub(super) fn count_year(dir: &Path, ext: Option<&str>) -> Result<usize> {
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
pub(super) fn read_dir_or_empty(
    dir: &Path,
) -> Result<Box<dyn Iterator<Item = io::Result<fs::DirEntry>>>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(Box::new(rd)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Box::new(std::iter::empty())),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", dir.display()))),
    }
}

/// Unfold and pick the value of the first line whose name (before any
/// `;PARAM=` or `:`) matches `key`. RFC 5545 lines can be folded across
/// multiple physical lines with a leading space or tab; iCalendar
/// consumers unfold before parsing.
pub(super) fn ics_field(body: &str, key: &str) -> Option<String> {
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

/// (filename, parsed JSON) for every `*.json` directly under `dir`.
pub(super) fn walk_flat_json(dir: &Path) -> Result<Vec<(String, Value)>> {
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
pub(super) fn walk_year_json(dir: &Path) -> Result<Vec<(String, String, Value)>> {
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

pub(super) fn read_json(path: &Path) -> Result<Value> {
    let body = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))
}

/// Try a series of keys and return the first non-empty string value.
pub(super) fn pick_str(value: &Value, keys: &[&str]) -> Option<String> {
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
pub(super) fn vendor_url(value: &Value) -> Option<String> {
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

pub(super) fn is_safe_http_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("http://") || s.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
