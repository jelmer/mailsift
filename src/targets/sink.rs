//! Building blocks shared by every artifact sink.
//!
//! - [`FileOutcome`]: what a sink did with one artifact (created or
//!   updated something at the returned location label, or kept what
//!   was already there).
//! - [`supersedes`]: the ordering rule every sink applies before
//!   replacing a record, so that the newest message wins regardless of
//!   the order messages are processed in.
//! - [`update_file`]: applies such a rule to one file atomically, so
//!   two messages about the same thing can't both find an older record
//!   and then write in either order. [`lock_dir`] is the lock under it.
//! - [`write_atomic`]: temp file + fsync + rename, so a partial write
//!   can't leave a truncated file in place.
//! - [`slugify`]: filesystem-safe ASCII slugger. `uppercase` is `true`
//!   for parcels (tracking numbers read better in caps); every other
//!   caller passes `false`.
//! - [`sanitize_ext`]: defend on-disk paths against weird extensions
//!   on the ticket / file sinks.
//! - [`sanitize_uid`]: defend on-disk paths against weird iCalendar
//!   UIDs on the local-events sink.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use tracing::info;

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
    /// The record at this location was filed from a newer message and
    /// was left alone.
    Kept(String),
}

/// Whether a record filed from a message dated `existing` must be kept
/// over one from a message dated `incoming`.
///
/// A mailbox is not processed in date order: a rescan, a re-filed
/// folder or an IMAP scan can all hand us an older mail after a newer
/// one. Only a strictly newer record is kept, so reprocessing the same
/// message still refreshes it. Without both dates there is nothing to
/// order by and the incoming message is filed.
pub fn supersedes(existing: Option<DateTime<Utc>>, incoming: Option<DateTime<Utc>>) -> bool {
    matches!((existing, incoming), (Some(existing), Some(incoming)) if existing > incoming)
}

/// Decides what to file at a location given what is there (`None` if
/// nothing): `Some(body)` to write, `None` to leave it alone. A remote
/// sink calls it again if the resource changed before the write landed.
pub type Merge<'a> = dyn Fn(Option<&[u8]>) -> Result<Option<Vec<u8>>> + 'a;

/// Exclusive hold on a sink directory until dropped.
pub struct DirLock {
    _dir: File,
}

/// Lock `dir`, creating it if needed, waiting for anyone else who
/// holds it. Hold the lock from looking at what is on file until the
/// write lands.
///
/// Messages are processed on several threads at once, and the milter,
/// a watcher and a one-off scan can all be running. The lock is an
/// advisory one on the directory itself, so it holds across all of
/// them and leaves nothing behind in the directory.
pub fn lock_dir(dir: &Path) -> Result<DirLock> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let handle = File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    handle
        .lock()
        .with_context(|| format!("locking {}", dir.display()))?;
    Ok(DirLock { _dir: handle })
}

/// Replace `target` with whatever `merge` makes of what is there, with
/// its directory locked throughout.
pub fn update_file(target: &Path, kind: &str, merge: &Merge<'_>) -> Result<FileOutcome> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("target {} has no parent dir", target.display()))?;
    let _lock = lock_dir(parent)?;
    let existing = read_if_exists(target)?;
    let Some(body) = merge(existing.as_deref())? else {
        return Ok(log_kept(target.display().to_string(), kind));
    };
    write_atomic(target, &body)?;
    Ok(log_file_outcome(target, existing.is_some(), kind))
}

/// Emit the `"<kind> kept"` log line and return the matching
/// [`FileOutcome`] for a record at `label` that was left alone.
pub fn log_kept(label: String, kind: &str) -> FileOutcome {
    info!(target = %label, "{kind} kept; already filed from a newer message");
    FileOutcome::Kept(label)
}

/// Read `path`, or `None` if nothing is there yet.
pub fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(body) => Ok(Some(body)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Emit the `"<kind> created"` or `"<kind> updated"` log line and
/// return the matching [`FileOutcome`] pointing at `target`. Every
/// local sink lands here after `write_atomic` to keep the log wording
/// consistent.
pub fn log_file_outcome(target: &Path, existed: bool, kind: &str) -> FileOutcome {
    let label = target.display().to_string();
    if existed {
        info!(target = %label, "{kind} updated");
        FileOutcome::Updated(label)
    } else {
        info!(target = %label, "{kind} created");
        FileOutcome::Created(label)
    }
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

    fn at(rfc3339: &str) -> Option<DateTime<Utc>> {
        Some(rfc3339.parse().unwrap())
    }

    #[test]
    fn supersedes_only_when_existing_is_strictly_newer() {
        let older = at("2025-10-28T10:00:00Z");
        let newer = at("2026-01-28T10:00:00Z");
        assert!(supersedes(newer, older));
        assert!(!supersedes(older, newer));
        assert!(!supersedes(newer, newer));
    }

    #[test]
    fn supersedes_needs_both_dates() {
        let dated = at("2026-01-28T10:00:00Z");
        assert!(!supersedes(dated, None));
        assert!(!supersedes(None, dated));
        assert!(!supersedes(None, None));
    }

    fn replace_with(body: &'static [u8]) -> impl Fn(Option<&[u8]>) -> Result<Option<Vec<u8>>> {
        move |_| Ok(Some(body.to_vec()))
    }

    #[test]
    fn update_file_creates_then_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("2026/record.json");

        let first = update_file(&target, "record", &replace_with(b"one")).unwrap();
        assert!(matches!(first, FileOutcome::Created(_)));
        let second = update_file(&target, "record", &replace_with(b"two")).unwrap();
        assert!(matches!(second, FileOutcome::Updated(_)));
        assert_eq!(fs::read(&target).unwrap(), b"two");
    }

    #[test]
    fn update_file_shows_the_merge_what_is_on_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("record.json");

        update_file(&target, "record", &|existing| {
            assert_eq!(existing, None);
            Ok(Some(b"one".to_vec()))
        })
        .unwrap();
        update_file(&target, "record", &|existing| {
            assert_eq!(existing, Some(b"one".as_slice()));
            Ok(Some([existing.unwrap(), b"+two"].concat()))
        })
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"one+two");
    }

    #[test]
    fn update_file_keeps_what_is_there_when_the_merge_declines() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("record.json");
        fs::write(&target, b"one").unwrap();

        let outcome = update_file(&target, "record", &|_| Ok(None)).unwrap();
        assert!(matches!(outcome, FileOutcome::Kept(_)));
        assert_eq!(fs::read(&target).unwrap(), b"one");

        // Declining when nothing is there leaves nothing there.
        let absent = tmp.path().join("absent.json");
        let outcome = update_file(&absent, "record", &|_| Ok(None)).unwrap();
        assert!(matches!(outcome, FileOutcome::Kept(_)));
        assert!(!absent.exists());
    }

    #[test]
    fn update_file_passes_on_a_merge_failure_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("record.json");
        fs::write(&target, b"one").unwrap();

        let err = update_file(&target, "record", &|_| bail!("can't merge")).unwrap_err();
        assert_eq!(err.to_string(), "can't merge");
        assert_eq!(fs::read(&target).unwrap(), b"one");
    }

    #[test]
    fn lock_dir_makes_a_second_taker_wait() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let got_it = AtomicBool::new(false);
        let lock = lock_dir(tmp.path()).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _lock = lock_dir(tmp.path()).unwrap();
                got_it.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!got_it.load(Ordering::SeqCst));
            drop(lock);
        });
        assert!(got_it.load(Ordering::SeqCst));
    }

    #[test]
    fn lock_dir_creates_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bills/2026");
        let _lock = lock_dir(&dir).unwrap();
        assert!(dir.is_dir());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn read_if_exists_distinguishes_missing_from_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("record.json");
        assert_eq!(read_if_exists(&path).unwrap(), None);
        fs::write(&path, b"{}").unwrap();
        assert_eq!(read_if_exists(&path).unwrap(), Some(b"{}".to_vec()));
    }

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
