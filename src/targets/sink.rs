//! Building blocks shared by every artifact sink.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow};

/// What a sink did with one artifact.
///
/// The payload is a human-readable location label.
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
    fn uid_keeps_safe_chars() {
        assert_eq!(sanitize_uid("invite-1@example.org"), "invite-1@example.org");
    }

    #[test]
    fn uid_folds_path_separators() {
        assert_eq!(sanitize_uid("../etc/passwd"), ".._etc_passwd");
    }

    #[test]
    fn uid_empty_becomes_underscore() {
        assert_eq!(sanitize_uid(""), "_");
    }
}
