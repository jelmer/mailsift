//! `seen.db`: the dedup store described in DESIGN.md §"Implementation".
//!
//! Keyed by `(kind, dedup_key)` (e.g. `(event, <iCalendar UID>)` or
//! `(parcel, <trackingNumber>)`), value is the content hash of the
//! last-filed payload. The pipeline checks the store before re-issuing
//! the expensive upstream calls (CalDAV PUT, Karrio register, 17track
//! register, Firefly POST/PUT) so a replay against a backlog doesn't
//! pummel third-party APIs for artifacts we've already filed.
//!
//! Local-filesystem targets (bills / receipts / tickets / subscriptions
//! / local events) are intentionally not gated: rewriting a small
//! JSON file is cheap and already idempotent (same key → same path),
//! and not consulting the store means a corrupt store can't make us
//! miss an update.
//!
//! ## Storage
//!
//! [`redb`]: pure-Rust, single-file, ACID. The whole API surface we
//! use is `open`, `begin_read` / `begin_write`, and one table lookup
//! per call. Concurrent milter tasks coordinate via redb's
//! single-writer / many-reader model; a write transaction is
//! microsecond-scale so contention is a non-issue at our message rate.
//!
//! Worst case if the store is corrupt or missing: we re-issue an
//! upstream call. Each gated target is idempotent (CalDAV PUT
//! replaces; Karrio/17track register are duplicate-safe; Firefly does
//! update-or-create), so a stale store costs at most some wasted
//! network round-trips, never lost data.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTableMetadata, TableDefinition};
use sha2::{Digest, Sha256};
use tracing::warn;

/// Single redb table holding every seen entry. The key is
/// `"<kind>:<dedup_key>"` (e.g. `"event:flight-fr1234@ryanair.com"`)
/// and the value is the lowercase-hex SHA-256 of the content payload.
///
/// One table for all kinds (rather than a table per kind) keeps the
/// open path simple and lets us scan the whole store in one pass if
/// we ever want a `seen list` debugging subcommand. Collision across
/// kinds is impossible because `kind` strings are disjoint.
const TABLE: TableDefinition<&str, &str> = TableDefinition::new("seen_v1");

/// Artifact kinds the dedup store distinguishes. Used as the
/// composite-key prefix; new kinds get a new variant rather than a
/// free-form string so we can grep for callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// CalDAV / local event UID.
    Event,
    /// Parcel `trackingNumber` (lowercased).
    Parcel,
    /// Bill `invoiceNumber` (or `payee+date` fallback).
    Bill,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Event => "event",
            Kind::Parcel => "parcel",
            Kind::Bill => "bill",
        }
    }
}

/// Compute the composite key `<kind>:<dedup_key>`. Centralised so the
/// reader and the writer can never disagree on the separator.
fn compose(kind: Kind, dedup_key: &str) -> String {
    format!("{}:{dedup_key}", kind.as_str())
}

/// Compute the content hash we store. Lowercase hex SHA-256; cheap to
/// generate, stable across mailsift restarts, and small enough that
/// the whole store stays well under a megabyte even with thousands of
/// entries.
pub fn hash(payload: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(payload);
    format!("{:x}", h.finalize())
}

/// The dedup store. Cloneable: wraps an `Arc<Database>` so the
/// milter's per-message tasks share a single open file handle.
#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
    /// Kept for log lines so a failing read/write tells the user which
    /// file is at fault.
    path: PathBuf,
}

impl Store {
    /// Open or create the redb file at `path`. The parent directory
    /// must already exist or be createable.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let db = Database::create(path)
            .with_context(|| format!("opening seen.db at {}", path.display()))?;
        // Ensure the table exists by opening it in a write txn; this
        // is a no-op after the first run.
        {
            let txn = db.begin_write().context("seen.db: begin_write")?;
            txn.open_table(TABLE).context("seen.db: open_table")?;
            txn.commit().context("seen.db: commit init txn")?;
        }
        Ok(Self {
            db: Arc::new(db),
            path: path.to_path_buf(),
        })
    }

    /// Default location: `$XDG_STATE_HOME/mailsift/seen.db`, falling
    /// back to `~/.local/state/mailsift/seen.db`. Returns `None` in
    /// the stripped-env case (no `$HOME`).
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
            })?;
        Some(base.join("mailsift").join("seen.db"))
    }

    /// Return `true` when the store already has `(kind, dedup_key)`
    /// recorded with this exact `content_hash`. The caller should
    /// skip its expensive upstream call when this returns `true`.
    ///
    /// Read failures are logged and treated as "not seen": a broken
    /// store mustn't silently suppress filings. Same idea as the
    /// stats recorder.
    pub fn is_seen(&self, kind: Kind, dedup_key: &str, content_hash: &str) -> bool {
        let composite = compose(kind, dedup_key);
        match self.lookup(&composite) {
            Ok(Some(stored)) => stored == content_hash,
            Ok(None) => false,
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    path = %self.path.display(),
                    "seen.db read failed; treating as not-seen"
                );
                false
            }
        }
    }

    fn lookup(&self, composite: &str) -> Result<Option<String>> {
        let txn = self.db.begin_read().context("seen.db: begin_read")?;
        let table = txn.open_table(TABLE).context("seen.db: open_table")?;
        Ok(table
            .get(composite)
            .context("seen.db: lookup")?
            .map(|v| v.value().to_string()))
    }

    /// Record `(kind, dedup_key) → content_hash`. Called from the
    /// gated targets after a successful upstream write. Failures are
    /// logged-and-swallowed (same rationale as `is_seen`): a broken
    /// store should not break extraction.
    pub fn mark(&self, kind: Kind, dedup_key: &str, content_hash: &str) {
        let composite = compose(kind, dedup_key);
        if let Err(e) = self.insert(&composite, content_hash) {
            warn!(
                error = format!("{e:#}"),
                path = %self.path.display(),
                key = %composite,
                "seen.db write failed; next replay will re-do this filing"
            );
        }
    }

    fn insert(&self, composite: &str, content_hash: &str) -> Result<()> {
        let txn = self.db.begin_write().context("seen.db: begin_write")?;
        {
            let mut table = txn.open_table(TABLE).context("seen.db: open_table")?;
            table
                .insert(composite, content_hash)
                .context("seen.db: insert")?;
        }
        txn.commit().context("seen.db: commit")?;
        Ok(())
    }

    /// Number of rows. Cheap because redb keeps a length counter per
    /// table; we don't iterate. Used by the `stats` subcommand to
    /// report `seen.db` size.
    #[allow(dead_code)] // used by upcoming `stats` integration
    pub fn len(&self) -> Result<u64> {
        let txn = self.db.begin_read().context("seen.db: begin_read")?;
        let table = txn.open_table(TABLE).context("seen.db: open_table")?;
        table.len().context("seen.db: len")
    }

    /// `true` when the store has no entries. Mirrors [`Self::len`]
    /// for the `len_without_is_empty` clippy lint; the implementation
    /// just delegates so we never have two divergent codepaths.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("seen.db");
        let s = Store::open(&path).unwrap();
        (dir, s)
    }

    #[test]
    fn unseen_key_returns_false() {
        let (_d, s) = store();
        assert!(!s.is_seen(Kind::Event, "uid-1", "abc"));
    }

    #[test]
    fn mark_then_is_seen() {
        let (_d, s) = store();
        s.mark(Kind::Event, "uid-1", "abc");
        assert!(s.is_seen(Kind::Event, "uid-1", "abc"));
    }

    #[test]
    fn different_hash_is_not_seen() {
        // The whole point of comparing content_hash, not just key:
        // an updated payload (e.g. flight rescheduled) must not be
        // skipped just because the UID is already in the store.
        let (_d, s) = store();
        s.mark(Kind::Event, "uid-1", "abc");
        assert!(!s.is_seen(Kind::Event, "uid-1", "xyz"));
    }

    #[test]
    fn different_kind_does_not_collide() {
        // A parcel with `trackingNumber = "uid-1"` and an event with
        // `UID = "uid-1"` must not shadow each other.
        let (_d, s) = store();
        s.mark(Kind::Event, "uid-1", "abc");
        assert!(!s.is_seen(Kind::Parcel, "uid-1", "abc"));
    }

    #[test]
    fn mark_overwrites_previous_hash() {
        // An update (same key, new hash) replaces the stored entry,
        // so a subsequent identical replay should then be is_seen.
        let (_d, s) = store();
        s.mark(Kind::Bill, "INV-1", "v1");
        s.mark(Kind::Bill, "INV-1", "v2");
        assert!(!s.is_seen(Kind::Bill, "INV-1", "v1"));
        assert!(s.is_seen(Kind::Bill, "INV-1", "v2"));
    }

    #[test]
    fn store_survives_reopen() {
        // ACID: closing and reopening must surface previously-marked
        // entries.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("seen.db");
        {
            let s = Store::open(&path).unwrap();
            s.mark(Kind::Event, "uid-1", "abc");
        }
        let s = Store::open(&path).unwrap();
        assert!(s.is_seen(Kind::Event, "uid-1", "abc"));
    }

    #[test]
    fn len_counts_entries() {
        let (_d, s) = store();
        assert_eq!(s.len().unwrap(), 0);
        s.mark(Kind::Event, "uid-1", "abc");
        s.mark(Kind::Parcel, "tn-1", "abc");
        s.mark(Kind::Event, "uid-1", "abc"); // overwrite, not insert
        assert_eq!(s.len().unwrap(), 2);
    }

    #[test]
    fn hash_is_stable_and_distinct() {
        assert_eq!(hash(b"hello"), hash(b"hello"));
        assert_ne!(hash(b"hello"), hash(b"world"));
        assert_eq!(hash(b"").len(), 64); // sha256 hex
    }

    #[test]
    fn default_path_uses_xdg_state_home() {
        // SAFETY: env mutations aren't thread-safe; libtest defaults
        // to one thread per process.
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state-seen");
            std::env::set_var("HOME", "/tmp/home-seen");
        }
        assert_eq!(
            Store::default_path(),
            Some(PathBuf::from("/tmp/xdg-state-seen/mailsift/seen.db"))
        );
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
        assert_eq!(
            Store::default_path(),
            Some(PathBuf::from(
                "/tmp/home-seen/.local/state/mailsift/seen.db"
            ))
        );
        unsafe {
            if let Some(v) = prev_state {
                std::env::set_var("XDG_STATE_HOME", v);
            }
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}
