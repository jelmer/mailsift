//! Building blocks shared by every artifact sink.
//!
//! - [`FileOutcome`]: what a sink did with one artifact (created or
//!   updated something at the returned location label).
//! - [`write_atomic`]: temp file + fsync + rename, so a partial write
//!   can't leave a truncated file in place.
//! - [`slugify`]: filesystem-safe ASCII slugger. `uppercase` is `true`
//!   for parcels (tracking numbers read better in caps); every other
//!   caller passes `false`.
//! - [`sanitize_ext`]: defend on-disk paths against weird extensions
//!   on the ticket / file sinks.
//! - [`sanitize_uid`]: defend on-disk paths against weird iCalendar
//!   UIDs on the local-events sink.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

/// What a sink did with one artifact.
///
/// The payload is a human-readable location label; a path string for
/// local sinks, a URL for WebDAV / CalDAV, a `"forwarded (...)"`
/// summary for the mail forwarder. Used for log lines and for the
/// `Summary` rendering in [`crate::pipeline`]; sinks pick the
/// representation that matches what they actually did.
#[derive(Debug)]
pub enum FileOutcome {
    /// New record landed at this location.
    Created(String),
    /// Existing record at this location was overwritten / re-sent.
    Updated(String),
}

/// Write `body` to `target`, creating any missing parent directories
/// and using an fsync'd rename so a partial write can't leave a
/// truncated file in place.
pub fn write_atomic(target: &Path, body: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("target {} has no parent dir", target.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating tmp file in {}", parent.display()))?;
    {
        let mut f = tmp.as_file();
        f.write_all(body).context("writing body to tmp file")?;
        f.sync_all().context("fsyncing tmp file")?;
    }
    tmp.persist(target)
        .map_err(|e| anyhow!("renaming tmp file into {}: {}", target.display(), e))?;
    Ok(())
}

/// Lowercase-or-uppercase, dash-collapsing slug for filesystem-safe
/// filenames. Keeps ASCII alphanumerics plus `_`, `.`, `+`; folds any
/// other byte to a single `-`; trims dashes at the edges.
pub fn slugify(s: &str, uppercase: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+') {
            let mapped = if uppercase {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            out.push(mapped);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Defend the on-disk path against weird extensions: no path separators,
/// no embedded dots, only ASCII alphanumerics. Returns the lowered form.
pub fn sanitize_ext(ext: &str) -> Result<String> {
    if ext.is_empty() {
        bail!("extension is empty");
    }
    if ext.contains('/') || ext.contains('\\') || ext.contains('.') {
        bail!("extension {ext:?} contains path-like characters");
    }
    if !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("extension {ext:?} must be ASCII alphanumeric");
    }
    Ok(ext.to_ascii_lowercase())
}

/// Sanitise an iCalendar UID for use as a filename. Keeps alphanumerics
/// plus `-`, `_`, `.`, `+`, `@`; folds anything else to `_`. Empty
/// inputs become `_` so the caller always gets a non-empty filename.
pub fn sanitize_uid(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len());
    for c in uid.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() { "_".to_string() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_lowercase_collapses_runs() {
        assert_eq!(
            slugify("Nederlandse Spoorwegen", false),
            "nederlandse-spoorwegen"
        );
        assert_eq!(slugify("NS // Reizigers!!", false), "ns-reizigers");
        assert_eq!(slugify("EasyJet Boarding!", false), "easyjet-boarding");
    }

    #[test]
    fn slug_uppercase_strips_spaces() {
        assert_eq!(slugify("1550 0806 521 781", true), "1550-0806-521-781");
        assert_eq!(slugify("tq566391606gb", true), "TQ566391606GB");
    }

    #[test]
    fn ext_validation_accepts_simple() {
        assert_eq!(sanitize_ext("PDF").unwrap(), "pdf");
        assert_eq!(sanitize_ext("pkpass").unwrap(), "pkpass");
    }

    #[test]
    fn ext_validation_rejects_unsafe() {
        assert!(sanitize_ext("").is_err());
        assert!(sanitize_ext("pdf/etc").is_err());
        assert!(sanitize_ext("p.df").is_err());
        assert!(sanitize_ext("pdf!").is_err());
    }

    #[test]
    fn uid_keeps_safe_chars() {
        assert_eq!(sanitize_uid("invite-1@example.org"), "invite-1@example.org");
    }

    #[test]
    fn uid_folds_path_separators() {
        // `/` isn't in the allow-list and becomes `_`. `.` is in the
        // allow-list, so `..` survives; caller is expected to use the
        // result as a filename component, not a full path.
        assert_eq!(sanitize_uid("../etc/passwd"), ".._etc_passwd");
    }

    #[test]
    fn uid_empty_becomes_underscore() {
        assert_eq!(sanitize_uid(""), "_");
    }
}
