//! Small shared HTTP-client helpers used by every HTTP target
//! (CalDAV, WebDAV, Firefly, Karrio, 17track).
//!
//! The auth-aware retry loop lives in [`super::http_auth`]; this
//! module just covers the boilerplate that even the simpler
//! token-authed sinks repeat: client construction with a sensible
//! timeout, plus a few error-shape helpers for the
//! "check HTTP status, format failure with truncated body" pattern
//! that every token-authed JSON sink (Firefly/Karrio/17track) repeats.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, Response};

/// Default request timeout for HTTP targets. 30s is enough for the
/// slowest CalDAV server we've seen while still short enough that a
/// hung remote doesn't stall the pipeline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Build a `reqwest::Client` with our standard timeout. `context_label`
/// appears in the error message if the build fails (very rare in
/// practice; only TLS init issues hit this path).
pub fn build_client(context_label: &str) -> Result<Client> {
    Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .with_context(|| format!("building {context_label} HTTP client"))
}

/// Build a `reqwest::Client` with an explicit timeout. Use this for
/// the WebDAV sink, which currently uses 60s because some servers are
/// slow to allocate an upload buffer.
pub fn build_client_with_timeout(context_label: &str, timeout: Duration) -> Result<Client> {
    Client::builder()
        .timeout(timeout)
        .build()
        .with_context(|| format!("building {context_label} HTTP client"))
}

/// Truncate a string for inclusion in an error message. Adds a `...`
/// suffix when truncation happens so the reader can tell the message
/// isn't complete.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

/// Reject an empty configuration value at sink construction. Replaces
/// the half-dozen `if x.is_empty() { bail!("X must not be empty"); }`
/// blocks that each token-authed sink (Firefly, Karrio, 17track) had to
/// carry for its URL and API token.
pub fn ensure_non_empty(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(())
}

/// Consume a [`reqwest::Response`]; if the status is non-success, build
/// the standard "sink POST <url> returned <status>: <body>" error with
/// a truncated body excerpt. On success, return the response body as a
/// `String` so the caller can parse it however it likes (`serde_json`,
/// or just discard).
///
/// The `op_label` is what shows up in the error message; typically
/// `"Firefly GET https://..."` or similar. Callers build it; we pair
/// it with the status and body excerpt.
pub async fn body_on_success(response: Response, op_label: &str) -> Result<String> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(body);
    }
    Err(anyhow!(
        "{op_label} returned {status}: {}",
        truncate(&body, 200)
    ))
}

/// Variant of [`body_on_success`] for endpoints whose success bodies
/// don't matter to the caller (POST /v1/trackers, PUT /api/v1/bills/{id}).
pub async fn discard_on_success(response: Response, op_label: &str) -> Result<()> {
    let _ = body_on_success(response, op_label).await?;
    Ok(())
}

/// Parse a JSON body returned from a successful response. Pairs with
/// [`body_on_success`] when the caller wants `serde_json` deserialization
/// rather than the raw string.
pub async fn json_on_success<T: serde::de::DeserializeOwned>(
    response: Response,
    op_label: &str,
) -> Result<T> {
    let body = body_on_success(response, op_label).await?;
    serde_json::from_str(&body).with_context(|| format!("parsing JSON from {op_label}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_passes_short_strings_unchanged() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_appends_ellipsis_when_clipping() {
        assert_eq!(truncate("abcdefghij", 5), "abcde...");
    }

    #[test]
    fn ensure_non_empty_accepts_non_empty() {
        ensure_non_empty("base URL", "https://example.org/").unwrap();
    }

    #[test]
    fn ensure_non_empty_rejects_empty() {
        let err = ensure_non_empty("Firefly API token", "").unwrap_err();
        assert_eq!(err.to_string(), "Firefly API token must not be empty");
    }
}
