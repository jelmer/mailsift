//! CLI parsing helpers: URL → connection params.
//!
//! Lives in the library (rather than next to `main.rs`) so tests can
//! exercise them.

use anyhow::{Context, Result, anyhow, bail};
use url::Url;

/// Connection parameters extracted from a CalDAV server URL.
#[derive(Debug, PartialEq, Eq)]
pub struct CaldavTarget {
    /// Server URL with any userinfo stripped, safe to pass to a
    /// generic HTTP client. The sink uses PROPFIND from here to
    /// discover the per-user collections.
    pub url: String,
    /// Username from the URL's userinfo, or `None`.
    pub user: Option<String>,
}

/// Parse a CalDAV URL of the form `https://[user@]host[/path/]`.
///
/// The userinfo (if any) is stripped from the URL passed to the HTTP
/// client; passwords in URLs are rejected since they end up in process
/// listings and shell history.
pub fn parse_caldav_url(input: &str) -> Result<CaldavTarget> {
    let mut url = Url::parse(input).with_context(|| format!("parsing CalDAV URL {input}"))?;
    match url.scheme() {
        "https" | "http" => {}
        other => {
            bail!("unsupported CalDAV scheme {other:?}: use https:// (or http:// for testing)")
        }
    }
    if url.password().is_some() {
        bail!("password in URL is not supported; use --caldav-password-file");
    }

    let user = if url.username().is_empty() {
        None
    } else {
        Some(
            percent_encoding::percent_decode_str(url.username())
                .decode_utf8()
                .context("decoding username")?
                .into_owned(),
        )
    };
    url.set_username("")
        .map_err(|_| anyhow!("could not clear userinfo on {input}"))?;

    Ok(CaldavTarget {
        url: url.to_string(),
        user,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caldav_url_with_user() {
        let t = parse_caldav_url("https://jelmer@cal.example.org/").unwrap();
        assert_eq!(t.url, "https://cal.example.org/");
        assert_eq!(t.user.as_deref(), Some("jelmer"));
    }

    #[test]
    fn caldav_url_without_user() {
        let t = parse_caldav_url("https://cal.example.org/").unwrap();
        assert_eq!(t.url, "https://cal.example.org/");
        assert!(t.user.is_none());
    }

    #[test]
    fn caldav_url_rejects_password() {
        let err = parse_caldav_url("https://u:pw@cal.example.org/").expect_err("password rejected");
        assert!(err.to_string().contains("password in URL"), "{err}");
    }

    #[test]
    fn caldav_url_rejects_non_http_scheme() {
        let err = parse_caldav_url("ftp://cal.example.org/").expect_err("scheme rejected");
        assert!(
            err.to_string().contains("unsupported CalDAV scheme"),
            "{err}"
        );
    }
}
