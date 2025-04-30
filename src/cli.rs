//! CLI parsing helpers: URL → connection params, current username from
//! the system password database, and the default config path discovery.
//!
//! These live in the library (rather than next to `main.rs`) so tests
//! can exercise them.

use std::ffi::CStr;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use tracing::warn;
use url::Url;

/// Connection parameters extracted from an `imap[s]://...` URL.
#[derive(Debug, PartialEq, Eq)]
pub struct ImapTarget {
    pub host: String,
    pub port: u16,
    /// Username from the URL's userinfo, or `None` if the URL had none
    /// (the caller can then default to the current OS user).
    pub user: Option<String>,
    /// Mailbox from the URL path. Empty path → `INBOX`. Leading slash
    /// and percent-encoding are stripped.
    pub mailbox: String,
}

/// Parse an IMAP URL of the form `imaps://[user@]host[:port]/[mailbox]`.
///
/// `imap://` is accepted but logs a warning; plaintext IMAP is almost
/// never what you want for a real account.
pub fn parse_imap_url(input: &str) -> Result<ImapTarget> {
    let url = Url::parse(input).with_context(|| format!("parsing IMAP URL {input}"))?;
    match url.scheme() {
        "imaps" => {}
        "imap" => {
            warn!(
                "imap:// URL uses plaintext; prefer imaps:// unless you really mean to (e.g. localhost dev server)"
            );
        }
        other => bail!("unsupported IMAP scheme {other:?}: use imaps:// (or imap://)"),
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("IMAP URL {input} has no host"))?
        .to_string();
    let port = url.port().unwrap_or(match url.scheme() {
        "imap" => 143,
        _ => 993,
    });

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

    if url.password().is_some() {
        // Refuse: passwords in URLs end up in process listings and shell
        // history. Use `--password-file` instead.
        bail!("password in URL is not supported; use --password-file");
    }

    let mailbox_path = url.path().trim_start_matches('/');
    let mailbox = if mailbox_path.is_empty() {
        "INBOX".to_string()
    } else {
        percent_encoding::percent_decode_str(mailbox_path)
            .decode_utf8()
            .context("decoding mailbox name")?
            .into_owned()
    };

    Ok(ImapTarget {
        host,
        port,
        user,
        mailbox,
    })
}

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
/// client; passwords in URLs are rejected for the same reason as IMAP.
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
    // Strip userinfo from the URL we hand to the HTTP client.
    url.set_username("")
        .map_err(|_| anyhow!("could not clear userinfo on {input}"))?;

    Ok(CaldavTarget {
        url: url.to_string(),
        user,
    })
}

/// Look up the current effective UID's username via `getpwuid_r`.
///
/// Returns `None` if the lookup fails (no entry, ENOMEM, etc.).
pub fn current_username() -> Option<String> {
    let uid = unsafe { libc::geteuid() };
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0i8; 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(pwd.pw_name) };
    name.to_str().ok().map(str::to_string)
}

/// Default config file path: `$XDG_CONFIG_HOME/mailsift/config.toml`
/// if `XDG_CONFIG_HOME` is set, otherwise `$HOME/.config/mailsift/config.toml`.
///
/// Returns `None` if neither variable is set (e.g. running as a system
/// daemon with a stripped environment).
pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("mailsift").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_url_full() {
        let t = parse_imap_url("imaps://jelmer@rhonwyn.jelmer.uk/INBOX").unwrap();
        assert_eq!(t.host, "rhonwyn.jelmer.uk");
        assert_eq!(t.port, 993);
        assert_eq!(t.user.as_deref(), Some("jelmer"));
        assert_eq!(t.mailbox, "INBOX");
    }

    #[test]
    fn imap_url_no_user_no_path() {
        let t = parse_imap_url("imaps://rhonwyn.jelmer.uk/").unwrap();
        assert_eq!(t.host, "rhonwyn.jelmer.uk");
        assert!(t.user.is_none());
        assert_eq!(t.mailbox, "INBOX");
    }

    #[test]
    fn imap_url_custom_port_and_path() {
        let t = parse_imap_url("imaps://u@host:1993/Archive/2024").unwrap();
        assert_eq!(t.host, "host");
        assert_eq!(t.port, 1993);
        assert_eq!(t.user.as_deref(), Some("u"));
        assert_eq!(t.mailbox, "Archive/2024");
    }

    #[test]
    fn imap_url_percent_decoded_mailbox() {
        let t = parse_imap_url("imaps://host/INBOX%2FSent").unwrap();
        assert_eq!(t.mailbox, "INBOX/Sent");
    }

    #[test]
    fn imap_url_plaintext_warns_but_parses() {
        let t = parse_imap_url("imap://host/").unwrap();
        assert_eq!(t.host, "host");
        // imap:// default port is 143.
        assert_eq!(t.port, 143);
    }

    #[test]
    fn imap_url_rejects_password() {
        let err = parse_imap_url("imaps://u:secret@host/").unwrap_err();
        assert!(err.to_string().contains("password in URL"), "{err}");
    }

    #[test]
    fn imap_url_rejects_other_scheme() {
        let err = parse_imap_url("https://host/").unwrap_err();
        assert!(err.to_string().contains("unsupported IMAP scheme"), "{err}");
    }

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
        let err = parse_caldav_url("https://u:pw@cal.example.org/").unwrap_err();
        assert!(err.to_string().contains("password in URL"), "{err}");
    }

    #[test]
    fn caldav_url_rejects_non_http_scheme() {
        let err = parse_caldav_url("ftp://cal.example.org/").unwrap_err();
        assert!(
            err.to_string().contains("unsupported CalDAV scheme"),
            "{err}"
        );
    }

    #[test]
    fn default_config_path_uses_xdg() {
        // SAFETY: env mutations are not thread-safe; tests in this crate
        // are run serially.
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg");
            std::env::set_var("HOME", "/tmp/home");
        }
        let p = default_config_path().unwrap();
        assert_eq!(p, PathBuf::from("/tmp/xdg/mailsift/config.toml"));

        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let p = default_config_path().unwrap();
        assert_eq!(p, PathBuf::from("/tmp/home/.config/mailsift/config.toml"));

        unsafe {
            if let Some(v) = prev_xdg {
                std::env::set_var("XDG_CONFIG_HOME", v);
            }
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}
