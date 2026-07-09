//! OAuth2 refresh-token handling for IMAP SASL `XOAUTH2`.
//!
//! The `XOAUTH2` SASL exchange itself needs a short-lived *access
//! token* (see [`crate::imap_scan`]). Those expire (Google's after ~1
//! hour), so a long-running `imap-scan --watch` can't be fed a fixed
//! token from a file: it would authenticate once and then fail on the
//! first reconnect after expiry.
//!
//! This module holds the long-lived *refresh token* plus the app's
//! client credentials and mints a fresh access token on demand via the
//! provider's token endpoint (RFC 6749 section 6, `grant_type=refresh_token`).
//! [`TokenProvider::access_token`] caches the minted token and only hits
//! the network again once it is close to expiry, so callers can ask for
//! a token before every connection without a round trip each time.
//!
//! Obtaining the refresh token in the first place needs a one-time
//! interactive consent flow; that is not implemented here yet (see the
//! TODO on [`TokenProvider`]). For now the refresh token is supplied out
//! of band (e.g. via `oauth2l` or a provider's playground) and read from
//! a file.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::debug;

use crate::targets::http_client::{body_on_success, build_client};

/// A known OAuth2 provider, carrying its token endpoint and the default
/// IMAP scope. Derived from the IMAP host where possible so the common
/// Gmail/Outlook cases need no explicit endpoint configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Google,
    Microsoft,
}

impl Provider {
    /// Map an IMAP hostname to a known provider. Returns `None` for
    /// hosts we don't recognise; those need an explicit token endpoint.
    pub fn from_imap_host(host: &str) -> Option<Provider> {
        let host = host.to_ascii_lowercase();
        match host.as_str() {
            "imap.gmail.com" | "imap.googlemail.com" => Some(Provider::Google),
            "outlook.office365.com" | "outlook.office.com" | "imap-mail.outlook.com" => {
                Some(Provider::Microsoft)
            }
            _ => None,
        }
    }

    /// Parse a provider named on the command line (`--oauth2-provider`).
    pub fn from_name(name: &str) -> Option<Provider> {
        match name.to_ascii_lowercase().as_str() {
            "google" | "gmail" => Some(Provider::Google),
            "microsoft" | "outlook" | "office365" => Some(Provider::Microsoft),
            _ => None,
        }
    }

    /// The provider's OAuth2 token endpoint.
    pub fn token_endpoint(self) -> &'static str {
        match self {
            // Microsoft's `common` tenant works for both personal and
            // work/school accounts; a single-tenant app would use its
            // tenant id instead, which the generic path (explicit
            // endpoint) covers.
            Provider::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            Provider::Google => "https://oauth2.googleapis.com/token",
        }
    }

    /// Default IMAP scope to request for this provider. Used by the
    /// (future) interactive flow; the refresh grant itself does not send
    /// a scope, inheriting the one the refresh token was granted with.
    pub fn default_scope(self) -> &'static str {
        match self {
            Provider::Google => "https://mail.google.com/",
            Provider::Microsoft => "https://outlook.office.com/IMAP.AccessAsUser.All",
        }
    }
}

/// Everything needed to mint access tokens from a refresh token.
///
/// `client_secret` is optional: Google desktop apps carry one, whereas
/// Microsoft public (native) clients must not send one. A generic
/// provider supplies its own `token_endpoint`.
#[derive(Debug, Clone)]
pub struct OAuth2Config {
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub refresh_token: String,
}

/// A cached access token and the instant it stops being usable.
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// A source of SASL `XOAUTH2` access tokens. The IMAP layer asks for a
/// token before every (re)connect without caring whether it is a fixed
/// pre-obtained token or one minted fresh from a refresh token.
///
/// `Send + Sync` because the IMAP config (which borrows a `TokenSource`)
/// is shared across the rayon worker pool during a scan.
pub trait TokenSource: Send + Sync {
    /// Return a currently-valid access token.
    fn access_token(&self) -> Result<String>;
}

/// A fixed, pre-obtained access token (e.g. from `oauth2l`). Hands back
/// the same token every time; suitable only for a run that finishes
/// within the token's lifetime, since it is never refreshed.
pub struct StaticTokenProvider {
    access_token: String,
}

impl StaticTokenProvider {
    pub fn new(access_token: String) -> Self {
        StaticTokenProvider { access_token }
    }
}

impl TokenSource for StaticTokenProvider {
    fn access_token(&self) -> Result<String> {
        Ok(self.access_token.clone())
    }
}

/// Mints and caches access tokens for SASL `XOAUTH2`.
///
/// Cheap to ask repeatedly: [`access_token`](TokenSource::access_token)
/// returns the cached token until it is within [`EXPIRY_MARGIN`] of
/// expiry, then refreshes. Refresh runs on the shared tokio runtime via
/// `block_on`, because the IMAP code path is blocking.
///
// TODO: add an interactive consent flow (`mailsift imap-auth`) to
// obtain the refresh token, rather than requiring it to be supplied out
// of band.
pub struct TokenProvider {
    config: OAuth2Config,
    runtime: tokio::runtime::Handle,
    cached: Mutex<Option<CachedToken>>,
}

/// How much lead time to leave before an access token's stated
/// expiry. Refreshing a little early avoids a race where the token is
/// still nominally valid when we send it but expires in flight, and
/// covers small clock differences between us and the provider.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// The subset of an OAuth2 token-endpoint response we consume. Other
/// fields (`token_type`, `scope`, `refresh_token`) are ignored; the
/// refresh grant does not rotate the refresh token for Google, and
/// Microsoft's rotation is handled by re-reading the file out of band.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Lifetime in seconds. Absent on some providers; we then assume a
    /// conservative default rather than caching indefinitely.
    expires_in: Option<u64>,
}

/// Fallback access-token lifetime when the token endpoint omits
/// `expires_in`. One hour matches Google and Microsoft defaults; erring
/// short just means an extra refresh, never a stale token.
const DEFAULT_EXPIRY: Duration = Duration::from_secs(3600);

impl TokenProvider {
    pub fn new(config: OAuth2Config, runtime: tokio::runtime::Handle) -> Self {
        TokenProvider {
            config,
            runtime,
            cached: Mutex::new(None),
        }
    }

    /// POST the `refresh_token` grant to the token endpoint and parse
    /// the response. Microsoft public clients must omit the secret;
    /// Google desktop apps include it.
    async fn refresh(&self) -> Result<TokenResponse> {
        let client = build_client("OAuth2 token")?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", self.config.refresh_token.as_str()),
            ("client_id", self.config.client_id.as_str()),
        ];
        if let Some(secret) = self.config.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let response = client
            .post(&self.config.token_endpoint)
            .form(&form)
            .send()
            .await
            .with_context(|| format!("POST {}", self.config.token_endpoint))?;
        let body = body_on_success(
            response,
            &format!("OAuth2 token endpoint {}", self.config.token_endpoint),
        )
        .await?;
        serde_json::from_str(&body)
            .with_context(|| format!("parsing token response from {}", self.config.token_endpoint))
    }
}

impl TokenSource for TokenProvider {
    /// Return a usable access token, refreshing if the cached one is
    /// missing or within [`EXPIRY_MARGIN`] of expiry.
    fn access_token(&self) -> Result<String> {
        let mut cached = self.cached.lock().expect("token cache mutex poisoned");
        if let Some(tok) = cached.as_ref()
            && tok.expires_at.saturating_duration_since(Instant::now()) > EXPIRY_MARGIN
        {
            return Ok(tok.access_token.clone());
        }

        let response = self
            .runtime
            .block_on(self.refresh())
            .context("refreshing OAuth2 access token")?;
        let lifetime = response
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_EXPIRY);
        // Stamp expiry against a single `now` taken after the network
        // round trip, so the in-flight time counts against the token's
        // life rather than being ignored.
        let expires_at = Instant::now() + lifetime;
        debug!(
            lifetime_secs = lifetime.as_secs(),
            "minted OAuth2 access token"
        );
        *cached = Some(CachedToken {
            access_token: response.access_token.clone(),
            expires_at,
        });
        Ok(response.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_from_gmail_host() {
        assert_eq!(
            Provider::from_imap_host("imap.gmail.com"),
            Some(Provider::Google)
        );
        assert_eq!(
            Provider::from_imap_host("IMAP.GMAIL.COM"),
            Some(Provider::Google)
        );
    }

    #[test]
    fn provider_from_outlook_host() {
        assert_eq!(
            Provider::from_imap_host("outlook.office365.com"),
            Some(Provider::Microsoft)
        );
    }

    #[test]
    fn provider_from_unknown_host() {
        assert_eq!(Provider::from_imap_host("imap.fastmail.com"), None);
    }

    #[test]
    fn provider_from_name_aliases() {
        assert_eq!(Provider::from_name("google"), Some(Provider::Google));
        assert_eq!(Provider::from_name("gmail"), Some(Provider::Google));
        assert_eq!(Provider::from_name("outlook"), Some(Provider::Microsoft));
        assert_eq!(Provider::from_name("office365"), Some(Provider::Microsoft));
        assert_eq!(Provider::from_name("fastmail"), None);
    }

    #[test]
    fn google_needs_no_tenant_in_endpoint() {
        assert_eq!(
            Provider::Google.token_endpoint(),
            "https://oauth2.googleapis.com/token"
        );
    }
}
