//! TOML configuration loading.
//!
//! Holds defaults for things otherwise passed on the command line:
//! extractor and bill directories, the local-events output dir, and the
//! CalDAV target. CLI arguments override any value loaded here.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Where extractor manifests live. `extractors_dir = "/path"` and
/// `extractors_dir = ["/a", "/b"]` both work; serde's `untagged` enum
/// covers both cases without extra glue.
///
/// When multiple directories list a manifest with the same `name:`,
/// the earlier directory wins, letting users layer a personal
/// directory of overrides on top of an upstream-shipped set.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ExtractorsDir {
    Single(PathBuf),
    Multiple(Vec<PathBuf>),
}

impl ExtractorsDir {
    pub fn as_slice(&self) -> &[PathBuf] {
        match self {
            ExtractorsDir::Single(p) => std::slice::from_ref(p),
            ExtractorsDir::Multiple(v) => v,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub extractors_dir: Option<ExtractorsDir>,
    #[serde(default)]
    pub bills_dir: Option<PathBuf>,
    #[serde(default)]
    pub parcels_dir: Option<PathBuf>,
    /// Directory under which to file `subscription` artifacts.
    #[serde(default)]
    pub subscriptions_dir: Option<PathBuf>,
    #[serde(default)]
    pub receipts_dir: Option<PathBuf>,
    /// Optional WebDAV collection to PUT `receipt` artifacts into.
    /// Mutually exclusive with `receipts_dir` and `[receipts_forward]`.
    #[serde(default)]
    pub receipts_webdav: Option<WebdavConfig>,
    /// Optional mail-forwarder for `receipt` artifacts. Mutually
    /// exclusive with `receipts_dir` and `[receipts_webdav]`.
    #[serde(default)]
    pub receipts_forward: Option<ReceiptForwardConfig>,
    #[serde(default)]
    pub tickets_dir: Option<PathBuf>,
    /// Optional WebDAV collection to PUT `ticket` artifacts into. Mutually
    /// exclusive with `tickets_dir`; the conflict is reported at startup.
    #[serde(default)]
    pub tickets_webdav: Option<WebdavConfig>,
    #[serde(default)]
    pub events_dir: Option<PathBuf>,
    #[serde(default)]
    pub caldav: Option<CaldavConfig>,
    #[serde(default)]
    pub karrio: Option<KarrioConfig>,
    #[serde(default)]
    pub seventeentrack: Option<SeventeenTrackConfig>,
    /// Firefly III bill-registration sink. When set, every filed bill
    /// is also registered with Firefly (update-or-create), so a
    /// re-emitted bill refreshes the Firefly record rather than
    /// duplicating it.
    #[serde(default)]
    pub firefly: Option<FireflyConfig>,
    /// Email addresses that are allowed to forward vendor mail to us.
    /// When the outer `From:` matches one of these and the mail
    /// carries a `message/rfc822` attachment, the pipeline acts on the
    /// inner message instead of the wrapper. DKIM is rechecked against
    /// the inner mail's own `Authentication-Results` header.
    #[serde(default)]
    pub trusted_forwarders: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FireflyConfig {
    /// Firefly base URL (e.g. `https://firefly.example.org/`).
    pub url: String,
    /// File containing a Firefly Personal Access Token.
    pub token_file: PathBuf,
}

/// Shared shape for any WebDAV-based artifact sink (today: tickets,
/// receipts). Mirrors [`CaldavConfig`] but without the schedule-inbox
/// discovery layer; WebDAV is a plain PUT target.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebdavConfig {
    /// Base collection URL. Sub-paths are appended for each artifact
    /// (e.g. `<base>/<year>/<slug>.<ext>`).
    pub url: String,
    /// Username for HTTP Basic. Optional: when omitted (along with
    /// `password_file`), the sink uses Kerberos/Negotiate from the
    /// caller's credential cache (requires the `gssapi` feature).
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password_file: Option<PathBuf>,
}

/// Mail-forwarder configuration for the receipts target. Wraps the
/// original message as a `message/rfc822` attachment and submits it
/// either via a local sendmail binary or via SMTP. Exactly one
/// transport (`sendmail` or `smtp_url`) must be set.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptForwardConfig {
    /// `From:` mailbox on the forwarded message
    /// (e.g. `mailsift@example.org`).
    pub from: String,
    /// One or more `To:` recipients.
    pub to: Vec<String>,
    /// Path to the sendmail binary. When set, sendmail is used.
    /// Mutually exclusive with `smtp_url`.
    #[serde(default)]
    pub sendmail: Option<PathBuf>,
    /// SMTP submission URL (`smtps://[user@]host[:port]`,
    /// `smtp://...`, `submissions://...`). Mutually exclusive with
    /// `sendmail`. Requires the `smtp` Cargo feature.
    #[serde(default)]
    pub smtp_url: Option<String>,
    /// Password file for SMTP submission. Required when the URL embeds
    /// a username; unused for sendmail.
    #[serde(default)]
    pub smtp_password_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KarrioConfig {
    pub url: String,
    pub token_file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeventeenTrackConfig {
    pub token_file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaldavConfig {
    /// CalDAV server URL. The sink runs PROPFIND requests against this
    /// to discover the current user's principal, schedule inbox, and
    /// default calendar, so the server root (e.g.
    /// `https://cal.example.org/`) is enough; pointing at a specific
    /// collection works too.
    pub url: String,
    /// CalDAV username. Optional: when omitted (along with
    /// `password_file`), the sink uses Kerberos/Negotiate auth from the
    /// caller's credential cache, provided the build has the `gssapi`
    /// feature.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password_file: Option<PathBuf>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let body = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&body).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Reject configs whose targets conflict before we get deep into
    /// CLI / sink construction. CLI flags can still take a different
    /// path; they override config wholesale and are validated where
    /// they're parsed.
    fn validate(&self) -> Result<()> {
        if self.events_dir.is_some() && self.caldav.is_some() {
            anyhow::bail!("config specifies both events_dir and [caldav]; pick one");
        }

        let receipts_variants = self.receipts_dir.is_some() as u8
            + self.receipts_webdav.is_some() as u8
            + self.receipts_forward.is_some() as u8;
        if receipts_variants > 1 {
            anyhow::bail!(
                "config specifies more than one of receipts_dir, [receipts_webdav], [receipts_forward]; pick one"
            );
        }

        if self.tickets_dir.is_some() && self.tickets_webdav.is_some() {
            anyhow::bail!("config specifies both tickets_dir and [tickets_webdav]; pick one");
        }

        if let Some(fwd) = &self.receipts_forward {
            match (fwd.sendmail.is_some(), fwd.smtp_url.is_some()) {
                (true, true) => anyhow::bail!(
                    "[receipts_forward] specifies both `sendmail` and `smtp_url`; pick one"
                ),
                (false, false) => {
                    anyhow::bail!("[receipts_forward] needs either `sendmail` or `smtp_url`")
                }
                _ => {}
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_example() {
        let body = r#"
extractors_dir = "/etc/mailsift/extractors"
bills_dir = "/home/jelmer/Documents/bills"
events_dir = "/home/jelmer/Documents/events"

[caldav]
url = "https://cal.example.org/"
user = "jelmer"
password_file = "/etc/mailsift/caldav.pass"
"#;
        let config: Config = toml::from_str(body).unwrap();
        assert_eq!(
            config.extractors_dir.as_ref().map(ExtractorsDir::as_slice),
            Some([PathBuf::from("/etc/mailsift/extractors")].as_slice())
        );
        assert_eq!(
            config.bills_dir,
            Some(PathBuf::from("/home/jelmer/Documents/bills"))
        );
        let caldav = config.caldav.unwrap();
        assert_eq!(caldav.url, "https://cal.example.org/");
        assert_eq!(caldav.user.as_deref(), Some("jelmer"));
        assert_eq!(
            caldav.password_file,
            Some(PathBuf::from("/etc/mailsift/caldav.pass"))
        );
    }

    #[test]
    fn parses_caldav_without_credentials() {
        let body = r#"
[caldav]
url = "https://cal.example.org/"
"#;
        let config: Config = toml::from_str(body).unwrap();
        let caldav = config.caldav.unwrap();
        assert_eq!(caldav.url, "https://cal.example.org/");
        assert!(caldav.user.is_none());
        assert!(caldav.password_file.is_none());
    }

    #[test]
    fn parses_tickets_webdav() {
        let body = r#"
[tickets_webdav]
url = "https://dav.example.org/tickets/"
user = "jelmer"
password_file = "/etc/mailsift/dav.pass"
"#;
        let config: Config = toml::from_str(body).unwrap();
        let dav = config.tickets_webdav.unwrap();
        assert_eq!(dav.url, "https://dav.example.org/tickets/");
        assert_eq!(dav.user.as_deref(), Some("jelmer"));
        assert_eq!(
            dav.password_file,
            Some(PathBuf::from("/etc/mailsift/dav.pass"))
        );
    }

    #[test]
    fn parses_tickets_webdav_without_credentials() {
        let body = r#"
[tickets_webdav]
url = "https://dav.example.org/tickets/"
"#;
        let config: Config = toml::from_str(body).unwrap();
        let dav = config.tickets_webdav.unwrap();
        assert_eq!(dav.url, "https://dav.example.org/tickets/");
        assert!(dav.user.is_none());
        assert!(dav.password_file.is_none());
    }

    #[test]
    fn parses_receipts_forward_sendmail() {
        let body = r#"
[receipts_forward]
from = "mailsift@example.org"
to = ["receipts@example.org"]
sendmail = "/usr/sbin/sendmail"
"#;
        let config: Config = toml::from_str(body).unwrap();
        let fwd = config.receipts_forward.unwrap();
        assert_eq!(fwd.from, "mailsift@example.org");
        assert_eq!(fwd.to, vec!["receipts@example.org".to_string()]);
        assert_eq!(fwd.sendmail, Some(PathBuf::from("/usr/sbin/sendmail")));
        assert!(fwd.smtp_url.is_none());
    }

    #[test]
    fn parses_firefly() {
        let body = r#"
[firefly]
url = "https://firefly.example.org/"
token_file = "/etc/mailsift/firefly.token"
"#;
        let config: Config = toml::from_str(body).unwrap();
        let firefly = config.firefly.unwrap();
        assert_eq!(firefly.url, "https://firefly.example.org/");
        assert_eq!(
            firefly.token_file,
            PathBuf::from("/etc/mailsift/firefly.token")
        );
    }

    #[test]
    fn parses_receipts_forward_smtp() {
        let body = r#"
[receipts_forward]
from = "mailsift@example.org"
to = ["receipts@example.org", "archive@example.org"]
smtp_url = "smtps://mailsift@mail.example.org/"
smtp_password_file = "/etc/mailsift/smtp.pass"
"#;
        let config: Config = toml::from_str(body).unwrap();
        let fwd = config.receipts_forward.unwrap();
        assert_eq!(fwd.to.len(), 2);
        assert_eq!(
            fwd.smtp_url.as_deref(),
            Some("smtps://mailsift@mail.example.org/")
        );
        assert!(fwd.sendmail.is_none());
    }

    #[test]
    fn extractors_dir_accepts_array() {
        let config: Config =
            toml::from_str(r#"extractors_dir = ["/a/extractors", "/b/extractors"]"#).unwrap();
        assert_eq!(
            config.extractors_dir.as_ref().map(ExtractorsDir::as_slice),
            Some(
                [
                    PathBuf::from("/a/extractors"),
                    PathBuf::from("/b/extractors")
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn empty_config_is_valid() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.extractors_dir.is_none());
        assert!(config.bills_dir.is_none());
        assert!(config.parcels_dir.is_none());
        assert!(config.events_dir.is_none());
        assert!(config.caldav.is_none());
    }

    #[test]
    fn rejects_unknown_key() {
        let body = r#"
bogus_key = "oops"
"#;
        assert!(toml::from_str::<Config>(body).is_err());
    }

    #[test]
    fn validate_rejects_events_dir_with_caldav() {
        let config: Config = toml::from_str(
            r#"
events_dir = "/tmp/events"
[caldav]
url = "https://cal.example.org/"
"#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("events_dir and [caldav]"), "{err}");
    }

    #[test]
    fn validate_rejects_two_receipts_variants() {
        let config: Config = toml::from_str(
            r#"
receipts_dir = "/tmp/receipts"
[receipts_webdav]
url = "https://dav.example.org/receipts/"
"#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("more than one of receipts"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_tickets_dir_with_webdav() {
        let config: Config = toml::from_str(
            r#"
tickets_dir = "/tmp/tickets"
[tickets_webdav]
url = "https://dav.example.org/tickets/"
"#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("tickets_dir and [tickets_webdav]"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_forward_with_both_transports() {
        let config: Config = toml::from_str(
            r#"
[receipts_forward]
from = "mailsift@example.org"
to = ["receipts@example.org"]
sendmail = "/usr/sbin/sendmail"
smtp_url = "smtps://mailsift@mail.example.org/"
"#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("sendmail` and `smtp_url"), "{err}");
    }

    #[test]
    fn validate_rejects_forward_without_transport() {
        let config: Config = toml::from_str(
            r#"
[receipts_forward]
from = "mailsift@example.org"
to = ["receipts@example.org"]
"#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("either `sendmail` or `smtp_url"),
            "{err}"
        );
    }
}
