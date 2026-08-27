//! Field-identification helpers shared by the JSON-artifact targets
//! (bills, receipts, parcels). Each of those targets parses a
//! `.<kind>.json` file, picks out a few identifying fields, and derives
//! a year from one of several possible date fields. The filesystem
//! bits (slugify, atomic write) live in [`super::sink`] alongside the
//! shared `FileOutcome`.

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

#[cfg(test)]
mod tests {
    use super::*;

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
