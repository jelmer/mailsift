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

use anyhow::{Context, Result, anyhow};
use reqwest::header::{HeaderMap, WWW_AUTHENTICATE};
use reqwest::{Client, RequestBuilder, Response, StatusCode};

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
/// server's `WWW-Authenticate` invites it. The closure builds a fresh
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
    let preferred = AttemptScheme::preferred(auth);
    let response = send_once(client, auth, &build_request, preferred).await?;
    if response.status() != StatusCode::UNAUTHORIZED {
        return Ok(response);
    }

    let challenge = parse_www_authenticate(response.headers());
    let Some(fallback) = AttemptScheme::fallback(auth, preferred, &challenge) else {
        return Ok(response);
    };
    send_once(client, auth, &build_request, fallback).await
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
}
