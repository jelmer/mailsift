//! HTTP authentication helpers shared by the CalDAV and WebDAV
//! targets.
//!
//! Both targets need the same machinery: an [`Auth`] value describing
//! what credentials are available, an [`AttemptScheme`] for what we'll
//! actually send on a given request, and a small retry-on-401 loop
//! that parses the server's `WWW-Authenticate` challenge to decide
//! whether to fall back to a different scheme.
//!
//! The auth flow:
//! - Send the preferred scheme preemptively (Negotiate when GSSAPI is
//!   built in, otherwise Basic when a password is configured).
//! - On 401: parse `WWW-Authenticate`. If we have a second scheme on
//!   offer and the server advertises it, retry once with that scheme.
//! - Otherwise return the original 401 to the caller, who decides
//!   whether to error or accept.

use std::time::Duration;

#[cfg(feature = "gssapi")]
use anyhow::anyhow;
use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, RETRY_AFTER, WWW_AUTHENTICATE};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use tracing::warn;

/// Maximum number of send attempts against a single request. One attempt
/// plus this many additional retries on transient failure.
const MAX_TRANSIENT_RETRIES: u32 = 3;

/// Base delay for exponential backoff on transient failures. Doubles on
/// each subsequent attempt; capped at [`MAX_BACKOFF`].
const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// Cap on the exponential-backoff delay. A `Retry-After` header from
/// the server can override this (up to a sanity limit).
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Absolute cap on any wait, even if the server sends a huge
/// `Retry-After`. A misconfigured server shouldn't be able to freeze
/// the daemon.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// HTTP auth credentials available to a sink.
///
/// Variants describe what we can try; the server's `WWW-Authenticate`
/// header decides what we actually send when a request is challenged.
#[derive(Debug, Clone)]
pub enum Auth {
    /// HTTP Basic only. The only option when the build has no GSSAPI
    /// support.
    #[cfg(not(feature = "gssapi"))]
    Basic { user: String, password: String },
    /// HTTP `Negotiate` (SPNEGO/Kerberos) only. `host` is the SPNEGO
    /// service host (the URL's authority).
    #[cfg(feature = "gssapi")]
    Negotiate { host: String },
    /// Both schemes available. Prefer Negotiate; fall back to Basic if
    /// the server rejects it.
    #[cfg(feature = "gssapi")]
    Both {
        user: String,
        password: String,
        host: String,
    },
}

/// Concrete auth scheme to attempt for a single request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptScheme {
    Basic,
    #[cfg(feature = "gssapi")]
    Negotiate,
}

impl AttemptScheme {
    fn preferred(auth: &Auth) -> Self {
        match auth {
            #[cfg(not(feature = "gssapi"))]
            Auth::Basic { .. } => Self::Basic,
            #[cfg(feature = "gssapi")]
            Auth::Negotiate { .. } => Self::Negotiate,
            #[cfg(feature = "gssapi")]
            Auth::Both { .. } => Self::Negotiate,
        }
    }

    /// Pick a fallback scheme to try after a 401, based on what we can
    /// offer and what the server's `WWW-Authenticate` header invites.
    #[cfg_attr(not(feature = "gssapi"), allow(unused_variables))]
    fn fallback(auth: &Auth, attempted: Self, challenge: &WwwAuthenticate) -> Option<Self> {
        match (auth, attempted) {
            #[cfg(feature = "gssapi")]
            (Auth::Both { .. }, Self::Negotiate) if challenge.basic => Some(Self::Basic),
            #[cfg(feature = "gssapi")]
            (Auth::Both { .. }, Self::Basic) if challenge.negotiate => Some(Self::Negotiate),
            _ => None,
        }
    }
}

/// Build an [`Auth`] from optional credentials and the URL we're
/// targeting. `target_label` is used in error messages (e.g. `"CalDAV"`,
/// `"WebDAV"`).
pub fn build_auth(
    base_url: &str,
    user: Option<String>,
    password: Option<String>,
    target_label: &str,
) -> Result<Auth> {
    let creds = match (user, password) {
        (Some(u), Some(p)) => Some((u, p)),
        (None, None) => None,
        (Some(_), None) => anyhow::bail!("{target_label} username supplied without a password"),
        (None, Some(_)) => anyhow::bail!("{target_label} password supplied without a username"),
    };

    #[cfg(feature = "gssapi")]
    {
        let host = extract_host(base_url, target_label)?;
        Ok(match creds {
            Some((user, password)) => Auth::Both {
                user,
                password,
                host,
            },
            None => Auth::Negotiate { host },
        })
    }
    #[cfg(not(feature = "gssapi"))]
    {
        let _ = base_url;
        match creds {
            Some((user, password)) => Ok(Auth::Basic { user, password }),
            None => anyhow::bail!(
                "{target_label} target requires a username and password (this build has no GSSAPI support)"
            ),
        }
    }
}

#[cfg(feature = "gssapi")]
fn extract_host(base_url: &str, target_label: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(base_url)
        .with_context(|| format!("parsing {target_label} URL {base_url}"))?;
    parsed
        .host_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{target_label} URL {base_url} has no host"))
}

fn basic_creds(auth: &Auth) -> Option<(&str, &str)> {
    match auth {
        #[cfg(not(feature = "gssapi"))]
        Auth::Basic { user, password } => Some((user, password)),
        #[cfg(feature = "gssapi")]
        Auth::Both { user, password, .. } => Some((user, password)),
        #[cfg(feature = "gssapi")]
        Auth::Negotiate { .. } => None,
    }
}

#[cfg(feature = "gssapi")]
fn negotiate_host(auth: &Auth) -> &str {
    match auth {
        Auth::Negotiate { host } | Auth::Both { host, .. } => host,
    }
}

/// Apply the chosen auth scheme to a request builder. The Negotiate
/// branch can fail (GSSAPI ticket lookup); Basic cannot.
pub fn apply_auth(
    req: RequestBuilder,
    auth: &Auth,
    scheme: AttemptScheme,
) -> Result<RequestBuilder> {
    match scheme {
        AttemptScheme::Basic => {
            let (user, password) =
                basic_creds(auth).expect("AttemptScheme::Basic implies basic creds available");
            Ok(req.basic_auth(user, Some(password)))
        }
        #[cfg(feature = "gssapi")]
        AttemptScheme::Negotiate => {
            let host = negotiate_host(auth);
            let token =
                crate::gss::spnego_token(host).context("building SPNEGO Negotiate token")?;
            Ok(req.header(reqwest::header::AUTHORIZATION, format!("Negotiate {token}")))
        }
    }
}

/// Send a request, retrying once with a fallback scheme on 401 when the
/// server's `WWW-Authenticate` invites it, and up to
/// [`MAX_TRANSIENT_RETRIES`] times with exponential backoff on transient
/// server errors (429, 500, 502, 503, 504). Honours `Retry-After` when
/// present (capped at [`MAX_RETRY_AFTER`]). The closure builds a fresh
/// request each time it's called (since `RequestBuilder` is consumed by
/// `send()`).
pub async fn send_with_auth_retry<F>(
    client: &Client,
    auth: &Auth,
    build_request: F,
) -> Result<Response>
where
    F: Fn(&Client) -> RequestBuilder,
{
    let mut attempt: u32 = 0;
    loop {
        let response = send_with_auth_only(client, auth, &build_request).await?;
        if !is_transient(response.status()) || attempt >= MAX_TRANSIENT_RETRIES {
            return Ok(response);
        }
        let wait = retry_after(response.headers()).unwrap_or_else(|| backoff(attempt));
        warn!(
            status = %response.status(),
            attempt = attempt + 1,
            max = MAX_TRANSIENT_RETRIES,
            wait_ms = wait.as_millis() as u64,
            "transient HTTP failure; retrying after backoff"
        );
        // Consume the body so the connection can be reused on the retry.
        drop(response);
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

/// The original auth-retry-only path, split out so the transient-retry
/// loop can invoke it once per attempt.
async fn send_with_auth_only<F>(client: &Client, auth: &Auth, build_request: &F) -> Result<Response>
where
    F: Fn(&Client) -> RequestBuilder,
{
    let preferred = AttemptScheme::preferred(auth);
    let response = send_once(client, auth, build_request, preferred).await?;
    if response.status() != StatusCode::UNAUTHORIZED {
        return Ok(response);
    }

    let challenge = parse_www_authenticate(response.headers());
    let Some(fallback) = AttemptScheme::fallback(auth, preferred, &challenge) else {
        return Ok(response);
    };
    send_once(client, auth, build_request, fallback).await
}

/// Which status codes we treat as safe to retry. 429 is the standard
/// rate-limit signal; the 5xx set is the transient family the client
/// can't distinguish from server bugs, so we err on the side of retrying.
fn is_transient(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Exponential backoff for attempt N (0-indexed): 0.5s, 1s, 2s, 4s...
/// clamped to [`MAX_BACKOFF`].
fn backoff(attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    BASE_BACKOFF.saturating_mul(factor).min(MAX_BACKOFF)
}

/// Parse a `Retry-After` header value. Supports the delay-in-seconds
/// form only; the HTTP-date form is technically valid but rare in
/// practice and adds an httpdate dep for a marginal case.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let secs: u64 = value.parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

async fn send_once<F>(
    client: &Client,
    auth: &Auth,
    build_request: &F,
    scheme: AttemptScheme,
) -> Result<Response>
where
    F: Fn(&Client) -> RequestBuilder,
{
    let req = build_request(client);
    let req = apply_auth(req, auth, scheme)?;
    req.send().await.context("sending HTTP request")
}

/// Schemes the server says it'll accept in its `WWW-Authenticate`
/// challenge. We only care about the two we know how to satisfy.
#[derive(Debug, Default, Clone, Copy)]
pub struct WwwAuthenticate {
    pub basic: bool,
    #[cfg_attr(not(feature = "gssapi"), allow(dead_code))]
    pub negotiate: bool,
}

pub fn parse_www_authenticate(headers: &HeaderMap) -> WwwAuthenticate {
    let mut out = WwwAuthenticate::default();
    for value in headers.get_all(WWW_AUTHENTICATE).iter() {
        let Ok(text) = value.to_str() else { continue };
        // A single header value can list multiple schemes
        // comma-separated, but each scheme name is the first token of
        // its entry, so we just check at word boundaries.
        for entry in text.split(',') {
            let trimmed = entry.trim_start();
            let scheme = trimmed.split_whitespace().next().unwrap_or("");
            if scheme.eq_ignore_ascii_case("Basic") {
                out.basic = true;
            } else if scheme.eq_ignore_ascii_case("Negotiate") {
                out.negotiate = true;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers_with(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(WWW_AUTHENTICATE, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn parse_single_basic_challenge() {
        let h = headers_with(&[r#"Basic realm="cal""#]);
        let p = parse_www_authenticate(&h);
        assert!(p.basic);
        assert!(!p.negotiate);
    }

    #[test]
    fn parse_negotiate_alone() {
        let h = headers_with(&["Negotiate"]);
        let p = parse_www_authenticate(&h);
        assert!(p.negotiate);
        assert!(!p.basic);
    }

    #[test]
    fn parse_comma_separated_both() {
        let h = headers_with(&[r#"Negotiate, Basic realm="cal""#]);
        let p = parse_www_authenticate(&h);
        assert!(p.basic);
        assert!(p.negotiate);
    }

    #[test]
    fn parse_two_separate_headers() {
        let h = headers_with(&["Negotiate", r#"Basic realm="cal""#]);
        let p = parse_www_authenticate(&h);
        assert!(p.basic);
        assert!(p.negotiate);
    }

    #[test]
    fn username_without_password_is_rejected() {
        let err = build_auth("https://example.org/", Some("u".into()), None, "WebDAV").unwrap_err();
        assert!(err.to_string().contains("without a password"), "{err}");
    }

    #[test]
    #[cfg(not(feature = "gssapi"))]
    fn missing_credentials_without_gssapi_is_rejected() {
        let err = build_auth("https://example.org/", None, None, "WebDAV").unwrap_err();
        assert!(err.to_string().contains("requires a username"), "{err}");
    }

    #[test]
    fn transient_codes_are_the_expected_set() {
        assert!(is_transient(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_transient(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_transient(StatusCode::BAD_GATEWAY));
        assert!(is_transient(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_transient(StatusCode::GATEWAY_TIMEOUT));
        assert!(!is_transient(StatusCode::OK));
        assert!(!is_transient(StatusCode::UNAUTHORIZED));
        assert!(!is_transient(StatusCode::BAD_REQUEST));
        assert!(!is_transient(StatusCode::NOT_FOUND));
        assert!(!is_transient(StatusCode::CONFLICT));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(0), BASE_BACKOFF);
        assert_eq!(backoff(1), BASE_BACKOFF * 2);
        assert_eq!(backoff(2), BASE_BACKOFF * 4);
        assert_eq!(backoff(3), BASE_BACKOFF * 8);
        // High attempt numbers saturate to the cap rather than overflowing.
        assert_eq!(backoff(20), MAX_BACKOFF);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn retry_after_parses_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(3)));
    }

    #[test]
    fn retry_after_caps_absurd_values() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("999999"));
        assert_eq!(retry_after(&h), Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn retry_after_ignores_http_date_form() {
        // We only support the delay-seconds form; a date-form value
        // returns None so the caller falls back to exponential backoff.
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h), None);
    }

    #[test]
    fn retry_after_absent_returns_none() {
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }
}
