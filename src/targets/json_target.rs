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
}
