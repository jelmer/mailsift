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
//! interactive consent flow; [`consent`] implements the loopback (and
//! manual-paste) authorization-code flow with PKCE and writes an
//! [`OAuth2Config`] bundle for later runs.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

    /// The provider's OAuth2 authorization endpoint, where the browser
    /// consent flow starts. Used by [`consent`].
    pub fn auth_endpoint(self) -> &'static str {
        match self {
            Provider::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            Provider::Google => "https://accounts.google.com/o/oauth2/v2/auth",
        }
    }

    /// The IMAP resource scope for this provider. The refresh grant
    /// itself does not send a scope, inheriting the one the refresh
    /// token was granted with; this is for reference and as the base of
    /// [`consent_scope`](Self::consent_scope).
    pub fn default_scope(self) -> &'static str {
        match self {
            Provider::Google => "https://mail.google.com/",
            Provider::Microsoft => "https://outlook.office.com/IMAP.AccessAsUser.All",
        }
    }

    /// Scope string to request during the interactive consent flow.
    /// Microsoft needs `offline_access` in the scope to return a refresh
    /// token; Google controls that with `access_type=offline` instead,
    /// so its consent scope is just the mail scope.
    pub fn consent_scope(self) -> &'static str {
        match self {
            Provider::Google => "https://mail.google.com/",
            Provider::Microsoft => {
                "offline_access https://outlook.office.com/IMAP.AccessAsUser.All"
            }
        }
    }
}

/// Everything needed to mint access tokens from a refresh token.
///
/// `client_secret` is optional: Google desktop apps carry one, whereas
/// Microsoft public (native) clients must not send one. A generic
/// provider supplies its own `token_endpoint`.
///
/// This is also the on-disk credential bundle written by `imap-auth`
/// and read by `imap-scan --oauth2-credentials-file`: it serialises to a
/// JSON object with these field names. `Debug` is redacted so the
/// refresh token and secret don't leak into logs or error output.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuth2Config {
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub refresh_token: String,
}

impl std::fmt::Debug for OAuth2Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2Config")
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

impl OAuth2Config {
    /// Load a credential bundle written by `imap-auth` from a JSON file.
    pub fn load(path: &std::path::Path) -> Result<OAuth2Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading OAuth2 credentials {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing OAuth2 credentials {}", path.display()))
    }

    /// Write the credential bundle as JSON, creating the file with
    /// owner-only permissions since it holds a refresh token.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("serialising OAuth2 credentials")?;
        write_private_file(path, json.as_bytes())
            .with_context(|| format!("writing OAuth2 credentials {}", path.display()))
    }
}

/// Write `contents` to `path`, restricting the file to the owner (mode
/// 0600 on Unix). The file holds a long-lived refresh token, so it must
/// not be world- or group-readable.
fn write_private_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(contents)?;
    Ok(())
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

/// A PKCE (RFC 7636) verifier and its S256 challenge. The verifier is a
/// high-entropy random string kept locally; the challenge (its SHA-256,
/// base64url-encoded) is sent in the authorization request. At token
/// exchange we present the verifier, proving we started the flow.
struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    fn generate() -> Result<Pkce> {
        let verifier = random_token(32)?;
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Ok(Pkce {
            verifier,
            challenge,
        })
    }
}

/// A base64url-encoded random token of `bytes` bytes of entropy. Used
/// for the PKCE verifier and the CSRF `state` value.
fn random_token(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow!("gathering randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// Parameters for the interactive consent flow.
pub struct ConsentRequest<'a> {
    pub auth_endpoint: &'a str,
    pub token_endpoint: &'a str,
    pub client_id: &'a str,
    /// Google desktop apps have a secret; Microsoft public clients don't.
    pub client_secret: Option<&'a str>,
    pub scope: &'a str,
    /// The account to pre-fill on the consent screen (`login_hint`).
    pub login_hint: Option<&'a str>,
    /// Skip the loopback server and browser: print the URL and read the
    /// redirected URL (or bare code) back from stdin. For headless / SSH
    /// sessions.
    pub no_browser: bool,
}

/// Run the interactive authorization-code flow and return a populated
/// [`OAuth2Config`] (including the refresh token) ready to persist.
///
/// Loopback mode (the default) starts a one-shot HTTP server on
/// `127.0.0.1`, opens the browser at the authorization URL, and waits
/// for the provider to redirect back with the code. `no_browser` mode
/// prints the URL and reads the redirect back from stdin instead, for
/// environments with no usable browser.
///
/// Runs on the supplied tokio runtime; the CLI entry point is blocking.
pub fn consent(req: ConsentRequest<'_>, runtime: &tokio::runtime::Handle) -> Result<OAuth2Config> {
    let pkce = Pkce::generate()?;
    let state = random_token(16)?;

    let (redirect_uri, code) = if req.no_browser {
        // Out-of-loop: the provider still needs a redirect_uri that is
        // registered for the client. Native clients register the
        // loopback range, so we use a fixed loopback URI the user's
        // browser will land on; they copy the resulting URL back.
        let redirect_uri = "http://127.0.0.1/".to_string();
        let auth_url = authorize_url(&req, &redirect_uri, &pkce.challenge, &state);
        println!("Open this URL in a browser and authorize:\n\n  {auth_url}\n");
        let code = read_code_from_stdin(&state)?;
        (redirect_uri, code)
    } else {
        runtime.block_on(loopback_consent(&req, &pkce.challenge, &state))?
    };

    let config = runtime.block_on(exchange_code(&req, &redirect_uri, &code, &pkce.verifier))?;
    Ok(config)
}

/// Build the authorization URL with PKCE and the CSRF `state`.
///
/// `access_type=offline` + `prompt=consent` (Google) force a refresh
/// token to be issued even on re-consent; Microsoft returns one whenever
/// the `offline_access` scope is present, which the caller includes.
fn authorize_url(
    req: &ConsentRequest<'_>,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut url =
        url::Url::parse(req.auth_endpoint).expect("provider auth endpoint is a valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", req.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", req.scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    if let Some(hint) = req.login_hint {
        url.query_pairs_mut().append_pair("login_hint", hint);
    }
    url.into()
}

/// Loopback consent: bind a port, open the browser, serve one request,
/// return the code. Returns the redirect URI actually used (it embeds
/// the chosen port) so the token exchange can echo it back.
async fn loopback_consent(
    req: &ConsentRequest<'_>,
    challenge: &str,
    state: &str,
) -> Result<(String, String)> {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding loopback listener for OAuth2 redirect")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/");

    let auth_url = authorize_url(req, &redirect_uri, challenge, state);
    if let Err(e) = open_browser(&auth_url) {
        debug!(error = %e, "could not open browser automatically");
    }
    println!("Authorize in your browser. If it didn't open, visit:\n\n  {auth_url}\n");

    let code = accept_redirect(&listener, state).await?;
    Ok((redirect_uri, code))
}

/// Accept a single HTTP request on `listener`, parse the OAuth2 redirect
/// query, validate the CSRF `state`, and return the authorization code.
/// Writes a small HTML page back so the browser shows a clean result.
async fn accept_redirect(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut socket, _) = listener
        .accept()
        .await
        .context("accepting OAuth2 redirect connection")?;

    // Read just the request line; the query string is all we need and it
    // fits comfortably in the first packet. A cap avoids an unbounded
    // read from a misbehaving client.
    let mut buf = vec![0u8; 8192];
    let n = socket
        .read(&mut buf)
        .await
        .context("reading OAuth2 redirect request")?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let target = request
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed OAuth2 redirect request"))?;

    let result = parse_redirect_query(target, expected_state);

    let body = match &result {
        Ok(_) => "mailsift: authorization received. You can close this tab.",
        Err(_) => "mailsift: authorization failed. Check the terminal.",
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;

    result
}

/// Extract and validate the authorization code from a redirect target,
/// which may be a full URL (`http://127.0.0.1:port/?code=...`) or just
/// the request path (`/?state=...&code=...`). Rejects a mismatched or
/// missing `state` (CSRF defence) and surfaces a provider `error=` if
/// present.
fn parse_redirect_query(target: &str, expected_state: &str) -> Result<String> {
    // Parse an absolute URL directly; join a relative path against a
    // dummy base so `Url` will accept it.
    let url = match url::Url::parse(target) {
        Ok(u) => u,
        Err(url::ParseError::RelativeUrlWithoutBase) => url::Url::parse("http://127.0.0.1/")
            .expect("static base URL is valid")
            .join(target)
            .context("parsing OAuth2 redirect URL")?,
        Err(e) => return Err(anyhow!("parsing OAuth2 redirect URL: {e}")),
    };

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }

    if let Some(err) = error {
        bail!("provider returned an OAuth2 error: {err}");
    }
    match state.as_deref() {
        Some(s) if s == expected_state => {}
        Some(_) => bail!("OAuth2 state mismatch; possible CSRF, aborting"),
        None => bail!("OAuth2 redirect had no state parameter"),
    }
    code.ok_or_else(|| anyhow!("OAuth2 redirect had no code parameter"))
}

/// Read a redirected URL or bare code from stdin (`no_browser` mode) and
/// extract the authorization code, validating `state` when a full URL
/// with a query is pasted.
fn read_code_from_stdin(expected_state: &str) -> Result<String> {
    use std::io::Write as _;

    print!("Paste the redirected URL (or the code): ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading pasted OAuth2 redirect")?;
    let line = line.trim();
    if line.is_empty() {
        bail!("no input given");
    }
    // A pasted full URL (or bare `?query`) carries state to validate; a
    // bare code doesn't, and we accept it as-is.
    if line.contains("code=") {
        parse_redirect_query(line, expected_state)
    } else {
        Ok(line.to_string())
    }
}

/// Exchange an authorization code for tokens, returning a populated
/// [`OAuth2Config`]. Errors if the provider doesn't return a refresh
/// token (e.g. re-consent without `prompt=consent`), since a bundle
/// without one is useless for later scans.
async fn exchange_code(
    req: &ConsentRequest<'_>,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<OAuth2Config> {
    let client = build_client("OAuth2 code exchange")?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", req.client_id),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = req.client_secret {
        form.push(("client_secret", secret));
    }
    let response = client
        .post(req.token_endpoint)
        .form(&form)
        .send()
        .await
        .with_context(|| format!("POST {}", req.token_endpoint))?;
    let body = body_on_success(
        response,
        &format!("OAuth2 token endpoint {}", req.token_endpoint),
    )
    .await?;

    #[derive(Deserialize)]
    struct CodeResponse {
        refresh_token: Option<String>,
    }
    let parsed: CodeResponse = serde_json::from_str(&body)
        .with_context(|| format!("parsing token response from {}", req.token_endpoint))?;
    let refresh_token = parsed.refresh_token.ok_or_else(|| {
        anyhow!(
            "provider did not return a refresh token; the account may have already granted \
             access without re-consent"
        )
    })?;

    Ok(OAuth2Config {
        token_endpoint: req.token_endpoint.to_string(),
        client_id: req.client_id.to_string(),
        client_secret: req.client_secret.map(str::to_string),
        refresh_token,
    })
}

/// Best-effort browser launch. Tries the platform opener; a failure is
/// non-fatal because the caller always prints the URL too.
fn open_browser(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        // xdg-open covers Linux/BSD desktops. Headless hosts won't have
        // it; that's fine, the URL is printed regardless.
        "xdg-open"
    };
    let status = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("launching {opener}"))?;
    if !status.success() {
        bail!("{opener} exited with {status}");
    }
    Ok(())
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

    #[test]
    fn microsoft_consent_scope_requests_offline_access() {
        // Microsoft only issues a refresh token when offline_access is
        // in the requested scope.
        assert_eq!(
            Provider::Microsoft.consent_scope(),
            "offline_access https://outlook.office.com/IMAP.AccessAsUser.All"
        );
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let pkce = Pkce::generate().unwrap();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        // base64url must not carry padding or +/ characters.
        assert!(!pkce.challenge.contains(['=', '+', '/']));
    }

    #[test]
    fn pkce_verifiers_differ_between_runs() {
        assert_ne!(
            Pkce::generate().unwrap().verifier,
            Pkce::generate().unwrap().verifier
        );
    }

    fn test_request() -> ConsentRequest<'static> {
        ConsentRequest {
            auth_endpoint: Provider::Google.auth_endpoint(),
            token_endpoint: Provider::Google.token_endpoint(),
            client_id: "cid.apps.googleusercontent.com",
            client_secret: Some("secret"),
            scope: Provider::Google.consent_scope(),
            login_hint: Some("you@gmail.com"),
            no_browser: false,
        }
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let url = authorize_url(
            &test_request(),
            "http://127.0.0.1:9999/",
            "CHALLENGE",
            "STATE",
        );
        let parsed = url::Url::parse(&url).unwrap();
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["code_challenge"], "CHALLENGE");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["state"], "STATE");
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:9999/");
        assert_eq!(params["access_type"], "offline");
        assert_eq!(params["login_hint"], "you@gmail.com");
    }

    #[test]
    fn parse_redirect_query_extracts_code() {
        let code = parse_redirect_query("/?state=abc&code=4/xyz", "abc").expect("valid redirect");
        assert_eq!(code, "4/xyz");
    }

    #[test]
    fn parse_redirect_query_accepts_full_url() {
        let code = parse_redirect_query("http://127.0.0.1:9999/?code=thecode&state=st", "st")
            .expect("valid redirect");
        assert_eq!(code, "thecode");
    }

    #[test]
    fn parse_redirect_query_rejects_state_mismatch() {
        let err = parse_redirect_query("/?state=wrong&code=x", "right").unwrap_err();
        assert_eq!(
            err.to_string(),
            "OAuth2 state mismatch; possible CSRF, aborting"
        );
    }

    #[test]
    fn parse_redirect_query_surfaces_provider_error() {
        let err = parse_redirect_query("/?error=access_denied", "st").unwrap_err();
        assert_eq!(
            err.to_string(),
            "provider returned an OAuth2 error: access_denied"
        );
    }

    #[test]
    fn credentials_json_round_trips() {
        let config = OAuth2Config {
            token_endpoint: "https://oauth2.googleapis.com/token".to_string(),
            client_id: "cid".to_string(),
            client_secret: Some("sek".to_string()),
            refresh_token: "1//rt".to_string(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: OAuth2Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.token_endpoint, config.token_endpoint);
        assert_eq!(back.client_id, config.client_id);
        assert_eq!(back.client_secret, config.client_secret);
        assert_eq!(back.refresh_token, config.refresh_token);
    }

    #[test]
    fn credentials_without_secret_omit_the_field() {
        let config = OAuth2Config {
            token_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/token"
                .to_string(),
            client_id: "cid".to_string(),
            client_secret: None,
            refresh_token: "rt".to_string(),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("client_secret"), "{json}");
        let back: OAuth2Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.client_secret, None);
    }

    #[test]
    fn debug_redacts_secrets() {
        let config = OAuth2Config {
            token_endpoint: "https://oauth2.googleapis.com/token".to_string(),
            client_id: "cid".to_string(),
            client_secret: Some("supersecret".to_string()),
            refresh_token: "1//verysecret".to_string(),
        };
        assert_eq!(
            format!("{config:?}"),
            "OAuth2Config { token_endpoint: \"https://oauth2.googleapis.com/token\", \
             client_id: \"cid\", client_secret: Some(\"<redacted>\"), \
             refresh_token: \"<redacted>\" }"
        );
    }
}
