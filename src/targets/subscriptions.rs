//! Local-directory target for `subscription` artifacts.
//!
//! Each `.subscription.json` artifact is parsed for its provider name
//! and filed under `<dir>/<slug>.json`. Unlike bills, subscriptions
//! don't carry a per-cycle invoice number; a subscription is a
//! recurring relationship, and re-running mailsift over a fresh
//! confirmation should refresh the existing record (renewal date,
//! current price) rather than file a sibling.
//!
//! The artifact JSON is schema.org-ish, but loose: extractors that
//! recognise an `Offer` with a `subscriptionDuration` field can emit
//! whatever they want, and unknown fields pass through.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tracing::info;

use super::FileOutcome;
use super::json_target::first_non_empty;
use super::sink::{slugify, write_atomic};

/// Shape we read out of a `.subscription.json` artifact. Loosely
/// schema.org-shaped (most fields mirror `Offer` / `Subscription`).
/// Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct Subscription {
    /// Display name of the subscription (e.g. `"Netflix"`). Either
    /// this or `provider` is required.
    name: Option<String>,
    /// Service provider (e.g. `"Netflix Inc."`). Falls back to `name`
    /// for the on-disk slug when one is missing.
    provider: Option<String>,
}

impl Subscription {
    fn identifier(&self) -> Option<&str> {
        first_non_empty([self.name.as_deref(), self.provider.as_deref()])
    }
}

pub fn file_subscription(src: &Path, dir: &Path) -> Result<FileOutcome> {
    let body = fs::read_to_string(src)
        .with_context(|| format!("reading subscription source {}", src.display()))?;
    let parsed: Subscription = serde_json::from_str(&body)
        .with_context(|| format!("parsing subscription JSON {}", src.display()))?;

    let ident = parsed
        .identifier()
        .ok_or_else(|| anyhow!("{}: missing 'name' or 'provider'", src.display()))?;
    let slug = slugify(ident, false);
    if slug.is_empty() {
        bail!(
            "{}: empty slug after sanitisation (name={:?})",
            src.display(),
            ident
        );
    }

    let target = dir.join(format!("{slug}.json"));
    let existed = target.exists();
    write_atomic(&target, body.as_bytes())?;

    let label = target.display().to_string();
    if existed {
        info!(target = %label, "subscription updated");
        Ok(FileOutcome::Updated(label))
    } else {
        info!(target = %label, "subscription created");
        Ok(FileOutcome::Created(label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_prefers_name() {
        let s: Subscription = serde_json::from_value(serde_json::json!({
            "name": "Netflix",
            "provider": "Netflix Inc.",
        }))
        .unwrap();
        assert_eq!(s.identifier(), Some("Netflix"));
    }

    #[test]
    fn identifier_falls_back_to_provider() {
        let s: Subscription = serde_json::from_value(serde_json::json!({
            "provider": "Disney Plus",
        }))
        .unwrap();
        assert_eq!(s.identifier(), Some("Disney Plus"));
    }

    #[test]
    fn file_subscription_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("incoming.subscription.json");
        fs::write(
            &src,
            r#"{"name":"Netflix","subscriptionDuration":"P1M","renewalDate":"2026-07-15"}"#,
        )
        .unwrap();
        let dir = tmp.path().join("subs");
        fs::create_dir_all(&dir).unwrap();
        let outcome = file_subscription(&src, &dir).unwrap();
        assert!(matches!(outcome, FileOutcome::Created(_)));
        let body = fs::read_to_string(dir.join("netflix.json")).unwrap();
        assert!(body.contains("Netflix"));
    }
}
