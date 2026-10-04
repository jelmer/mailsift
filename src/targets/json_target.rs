//! Field-identification helpers shared by the JSON-artifact targets
//! (bills, receipts, parcels). Each of those targets parses a
//! `.<kind>.json` file, picks out a few identifying fields, and derives
//! a year from one of several possible date fields. The filesystem
//! bits (slugify, atomic write) live in [`super::sink`] alongside the
//! shared `FileOutcome`.
//!
//! Every filed record carries the date of the message it came from in
//! `receivedAt`. [`write_unless_newer`] and friends compare that
//! against an incoming record so an older message never replaces what
//! a newer one filed.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tracing::warn;

use super::FileOutcome;
use super::sink::{log_file_outcome, log_kept, read_if_exists, supersedes, write_atomic};

/// Read a JSON artifact from disk and parse it as `T`, keeping the raw
/// body so the caller can also write it back out or transform it. Error
/// messages name `kind` (`"bill"`, `"receipt"`, ...) so they carry
/// through to the pipeline log.
pub fn read_and_parse<T>(src: &Path, kind: &str) -> Result<(String, T)>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let body = fs::read_to_string(src)
        .with_context(|| format!("reading {kind} source {}", src.display()))?;
    let parsed = serde_json::from_str(&body)
        .with_context(|| format!("parsing {kind} JSON {}", src.display()))?;
    Ok((body, parsed))
}

/// First non-empty (after trim) entry from a small list of candidates.
pub fn first_non_empty<const N: usize>(candidates: [Option<&str>; N]) -> Option<&str> {
    candidates.into_iter().flatten().find_map(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t) }
    })
}

/// Year prefix of an ISO-ish date string. Reads the first four chars
/// and parses them as a year; returns `None` if they don't look like
/// one. The schema.org dates we deal with all start with `YYYY-...`.
pub fn year_from_iso_prefix(s: &str) -> Option<i32> {
    let prefix = s.trim().get(..4)?;
    let y: i32 = prefix.parse().ok()?;
    (1970..=9999).contains(&y).then_some(y)
}

/// Pick the first parseable year from a list of date candidates,
/// falling back to the current calendar year when nothing parses.
pub fn derive_year<'a, I>(candidates: I) -> i32
where
    I: IntoIterator<Item = Option<&'a str>>,
{
    for candidate in candidates.into_iter().flatten() {
        if let Some(y) = year_from_iso_prefix(candidate) {
            return y;
        }
    }
    use chrono::Datelike;
    chrono::Utc::now().year()
}

/// Format a unix timestamp as an RFC3339 string for use as a
/// `receivedAt` field.
pub fn format_received_at(epoch: i64) -> Option<String> {
    let dt = chrono::DateTime::from_timestamp(epoch, 0)?;
    Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Inject `receivedAt = <RFC3339 string>` into the JSON body iff the
/// field isn't already set (extractor precedence wins). Silently
/// returns the input untouched when the body isn't a JSON object or
/// when no epoch is supplied.
pub fn body_with_received_at(body: &str, received_at_epoch: Option<i64>) -> String {
    let Some(epoch) = received_at_epoch else {
        return body.to_string();
    };
    let Some(stamped) = format_received_at(epoch) else {
        return body.to_string();
    };
    let mut value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return body.to_string(),
    };
    if let Some(obj) = value.as_object_mut()
        && !obj.contains_key("receivedAt")
    {
        obj.insert("receivedAt".into(), serde_json::Value::String(stamped));
        return serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_string());
    }
    body.to_string()
}

/// The `receivedAt` of a single record or history entry, as a
/// comparable timestamp.
pub fn received_at_of(obj: &Map<String, Value>) -> Option<DateTime<Utc>> {
    let raw = obj.get("receivedAt")?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// The `receivedAt` of a JSON record body: the date of the message it
/// was filed from. `None` when the record doesn't carry a usable one.
pub fn received_at_in(body: &[u8]) -> Result<Option<DateTime<Utc>>> {
    let value: Value = serde_json::from_slice(body).context("parsing record JSON")?;
    Ok(value.as_object().and_then(received_at_of))
}

/// Whether the record body `existing`, found at `label`, was filed
/// from a newer message than one dated `incoming`.
///
/// A record we can't parse has no date to order by, so it never wins:
/// replacing a corrupt record is better than keeping it forever.
pub fn is_from_newer(existing: &[u8], label: &str, incoming: Option<DateTime<Utc>>) -> bool {
    let existing = received_at_in(existing).unwrap_or_else(|e| {
        warn!(target = %label, error = format!("{e:#}"), "existing record is unreadable; replacing");
        None
    });
    supersedes(existing, incoming)
}

/// Whether the record at `target`, if any, was filed from a newer
/// message than one dated `incoming`.
pub fn filed_from_newer(target: &Path, incoming: Option<DateTime<Utc>>) -> Result<bool> {
    Ok(read_if_exists(target)?
        .is_some_and(|existing| is_from_newer(&existing, &target.display().to_string(), incoming)))
}

/// Write the JSON record `body` to `target`, unless the record already
/// there was filed from a newer message.
pub fn write_unless_newer(target: &Path, body: &str, kind: &str) -> Result<FileOutcome> {
    let label = target.display().to_string();
    let incoming = received_at_in(body.as_bytes())?;
    let existing = read_if_exists(target)?;
    if let Some(existing) = &existing
        && is_from_newer(existing, &label, incoming)
    {
        return Ok(log_kept(label, kind));
    }
    write_atomic(target, body.as_bytes())?;
    Ok(log_file_outcome(target, existing.is_some(), kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(received_at: &str, price: f64) -> String {
        serde_json::json!({"name": "Spotify", "price": price, "receivedAt": received_at})
            .to_string()
    }

    fn price_at(path: &Path) -> Value {
        let v: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        v["price"].clone()
    }

    #[test]
    fn write_unless_newer_keeps_record_from_newer_message() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("spotify.json");
        let newer = record("2026-01-28T10:00:00Z", 12.99);
        let older = record("2025-10-28T10:00:00Z", 11.99);

        let first = write_unless_newer(&target, &newer, "subscription").unwrap();
        assert!(matches!(first, FileOutcome::Created(_)));
        let second = write_unless_newer(&target, &older, "subscription").unwrap();
        assert!(matches!(second, FileOutcome::Kept(_)));
        assert_eq!(price_at(&target), 12.99);
    }

    #[test]
    fn write_unless_newer_replaces_record_from_older_message() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("spotify.json");
        write_unless_newer(
            &target,
            &record("2025-10-28T10:00:00Z", 11.99),
            "subscription",
        )
        .unwrap();
        let outcome = write_unless_newer(
            &target,
            &record("2026-01-28T10:00:00Z", 12.99),
            "subscription",
        )
        .unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        assert_eq!(price_at(&target), 12.99);
    }

    #[test]
    fn write_unless_newer_refreshes_record_from_same_message() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("spotify.json");
        write_unless_newer(
            &target,
            &record("2026-01-28T10:00:00Z", 11.99),
            "subscription",
        )
        .unwrap();
        let outcome = write_unless_newer(
            &target,
            &record("2026-01-28T10:00:00Z", 12.99),
            "subscription",
        )
        .unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        assert_eq!(price_at(&target), 12.99);
    }

    #[test]
    fn write_unless_newer_replaces_undated_record() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("spotify.json");
        fs::write(&target, r#"{"name":"Spotify","price":9.99}"#).unwrap();
        let outcome = write_unless_newer(
            &target,
            &record("2025-10-28T10:00:00Z", 11.99),
            "subscription",
        )
        .unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        assert_eq!(price_at(&target), 11.99);
    }

    #[test]
    fn write_unless_newer_replaces_unreadable_record() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("spotify.json");
        fs::write(&target, "{ truncated").unwrap();
        let outcome = write_unless_newer(
            &target,
            &record("2025-10-28T10:00:00Z", 11.99),
            "subscription",
        )
        .unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        assert_eq!(price_at(&target), 11.99);
    }

    #[test]
    fn received_at_in_normalises_offsets() {
        let body = br#"{"receivedAt":"2024-12-20T09:00:00+02:00"}"#;
        assert_eq!(
            received_at_in(body).unwrap(),
            Some("2024-12-20T07:00:00Z".parse().unwrap())
        );
        assert_eq!(received_at_in(b"{}").unwrap(), None);
        assert_eq!(received_at_in(b"42").unwrap(), None);
    }

    #[test]
    fn first_non_empty_skips_blanks() {
        assert_eq!(
            first_non_empty([None, Some("  "), Some("hit"), Some("later")]),
            Some("hit")
        );
        assert_eq!(first_non_empty::<3>([None, None, None]), None);
    }

    #[test]
    fn year_prefix_extracts_year() {
        assert_eq!(year_from_iso_prefix("2026-06-27"), Some(2026));
        assert_eq!(year_from_iso_prefix("1969-01-01"), None);
        assert_eq!(year_from_iso_prefix("abcd"), None);
    }

    #[test]
    fn derive_year_falls_back_to_current() {
        use chrono::Datelike;
        let now = chrono::Utc::now().year();
        assert_eq!(derive_year::<[Option<&str>; 0]>([]), now);
        assert_eq!(derive_year([None, Some("nope")]), now);
    }

    #[test]
    fn derive_year_picks_first_parseable() {
        assert_eq!(derive_year([Some("bad"), Some("2024-01-01")]), 2024);
    }

    #[test]
    fn format_received_at_produces_rfc3339() {
        // 2026-08-27T09:30:00Z
        assert_eq!(
            format_received_at(1787823000).as_deref(),
            Some("2026-08-27T09:30:00Z")
        );
    }

    #[test]
    fn body_with_received_at_injects_field_once() {
        let body = r#"{"payee":"Acme","invoiceNumber":"INV1"}"#;
        let stamped = body_with_received_at(body, Some(1787823000));
        let v: serde_json::Value = serde_json::from_str(&stamped).unwrap();
        assert_eq!(v["receivedAt"], "2026-08-27T09:30:00Z");
        assert_eq!(v["payee"], "Acme");
    }

    #[test]
    fn body_with_received_at_preserves_existing() {
        let body = r#"{"payee":"Acme","receivedAt":"2020-01-01T00:00:00Z"}"#;
        let stamped = body_with_received_at(body, Some(1787823000));
        let v: serde_json::Value = serde_json::from_str(&stamped).unwrap();
        assert_eq!(v["receivedAt"], "2020-01-01T00:00:00Z");
    }

    #[test]
    fn body_with_received_at_noop_without_epoch() {
        let body = r#"{"payee":"Acme"}"#;
        assert_eq!(body_with_received_at(body, None), body);
    }

    #[test]
    fn body_with_received_at_noop_when_not_object() {
        let body = "42";
        let stamped = body_with_received_at(body, Some(1787823000));
        assert_eq!(stamped, "42");
    }
}
