//! Mail forwarding target for `receipt` artifacts.
//!
//! Builds a `multipart/mixed` message that wraps the original RFC822
//! source as a `message/rfc822` attachment, with a short text/plain
//! body summarising what was extracted. Delivers either via `sendmail`
//! (a local MTA pipe) or via SMTP submission.
//!
//! Transports:
//! - [`MailForwarder::Sendmail`]: always available. Spawns a sendmail
//!   binary (`/usr/sbin/sendmail` by default) and writes the wire
//!   bytes to its stdin. The system MTA handles delivery.
//! - [`MailForwarder::Smtp`]: gated behind the `smtp` Cargo feature.
//!   Talks directly to an SMTP submission server (`smtp://` /
//!   `smtps://` / `submissions://`).
//!
//! Both transports run on the tokio runtime supplied by the caller;
//! mirroring the CalDAV / WebDAV pattern, so a `forward` call from the
//! synchronous pipeline blocks the calling thread on the runtime
//! handle.

use anyhow::{Context, Result, anyhow};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use lettre::{AsyncSendmailTransport, AsyncTransport, Tokio1Executor};
use tokio::runtime::Handle;
use tracing::info;

use super::sink::slugify;

/// Where to send the forwarded receipt.
pub enum MailForwarder {
    Sendmail {
        from: Mailbox,
        to: Vec<Mailbox>,
        runtime: Handle,
        /// Optional override for the sendmail binary path. `None`
        /// defaults to `/usr/sbin/sendmail`.
        command: Option<String>,
    },
    #[cfg(feature = "smtp")]
    Smtp {
        from: Mailbox,
        to: Vec<Mailbox>,
        runtime: Handle,
        transport: lettre::AsyncSmtpTransport<Tokio1Executor>,
    },
}

impl MailForwarder {
    /// Build a sendmail forwarder.
    pub fn sendmail(
        from: &str,
        to: Vec<String>,
        command: Option<String>,
        runtime: Handle,
    ) -> Result<Self> {
        let from = parse_mailbox(from).context("parsing `from` address")?;
        let to = parse_recipients(&to)?;
        Ok(MailForwarder::Sendmail {
            from,
            to,
            runtime,
            command,
        })
    }

    /// Build an SMTP forwarder.
    ///
    /// `url` is a lettre-style submission URL
    /// (`smtps://[user@]host[:port]`, `smtp://`, `submissions://`).
    /// `password` is required when the URL embeds a user.
    #[cfg(feature = "smtp")]
    pub fn smtp(
        from: &str,
        to: Vec<String>,
        url: &str,
        password: Option<String>,
        runtime: Handle,
    ) -> Result<Self> {
        use lettre::AsyncSmtpTransport;
        use lettre::transport::smtp::authentication::Credentials;

        let from = parse_mailbox(from).context("parsing `from` address")?;
        let to = parse_recipients(&to)?;

        let parsed = url::Url::parse(url).with_context(|| format!("parsing SMTP URL {url}"))?;
        let username = if parsed.username().is_empty() {
            None
        } else {
            Some(
                percent_encoding::percent_decode_str(parsed.username())
                    .decode_utf8()
                    .context("decoding SMTP username")?
                    .into_owned(),
            )
        };
        if parsed.password().is_some() {
            anyhow::bail!(
                "password in SMTP URL is not supported; use --receipts-forward-smtp-password-file"
            );
        }
        let credentials = match (username, password) {
            (Some(u), Some(p)) => Some(Credentials::new(u, p)),
            (Some(_), None) => {
                anyhow::bail!("SMTP URL contains a username but no password file was supplied")
            }
            (None, Some(_)) => {
                anyhow::bail!("SMTP password supplied without a username in the URL")
            }
            (None, None) => None,
        };

        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .with_context(|| format!("building SMTP transport for {url}"))?;
        if let Some(creds) = credentials {
            builder = builder.credentials(creds);
        }
        Ok(MailForwarder::Smtp {
            from,
            to,
            runtime,
            transport: builder.build(),
        })
    }

    /// Forward a receipt artifact: wrap the original `raw_message` as a
    /// `message/rfc822` attachment and submit it.
    pub fn forward(&self, raw_message: &[u8], subject_hint: &str) -> Result<()> {
        let message = self.build_message(raw_message, subject_hint)?;
        let wire = message.formatted();

        match self {
            MailForwarder::Sendmail {
                runtime, command, ..
            } => {
                let transport = match command {
                    Some(c) => AsyncSendmailTransport::<Tokio1Executor>::new_with_command(c),
                    None => AsyncSendmailTransport::<Tokio1Executor>::new(),
                };
                runtime
                    .block_on(transport.send_raw(message.envelope(), &wire))
                    .context("submitting via sendmail")?;
            }
            #[cfg(feature = "smtp")]
            MailForwarder::Smtp {
                runtime, transport, ..
            } => {
                runtime
                    .block_on(transport.send_raw(message.envelope(), &wire))
                    .context("submitting via SMTP")?;
            }
        }
        info!(subject = subject_hint, "forwarded receipt");
        Ok(())
    }

    fn build_message(&self, raw_message: &[u8], subject_hint: &str) -> Result<Message> {
        let (from, to) = match self {
            MailForwarder::Sendmail { from, to, .. } => (from, to),
            #[cfg(feature = "smtp")]
            MailForwarder::Smtp { from, to, .. } => (from, to),
        };
        let mut builder = Message::builder()
            .from(from.clone())
            .subject(format!("[receipt] {subject_hint}"));
        for recipient in to {
            builder = builder.to(recipient.clone());
        }

        let body = SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(format!(
                "Receipt extracted from email subject: {subject_hint}\n\
                 \n\
                 The original message is attached as message/rfc822.\n"
            ));

        // `Attachment::new_inline` plus `message/rfc822` content type
        // gives the desired "forward as attachment" semantics. Recipient
        // clients render the inner message as a nested mail.
        let attached = Attachment::new(format!("{}-original.eml", attachment_slug(subject_hint)))
            .body(
                raw_message.to_vec(),
                ContentType::parse("message/rfc822").expect("static content type is valid"),
            );

        let multipart = MultiPart::mixed().singlepart(body).singlepart(attached);
        builder
            .multipart(multipart)
            .map_err(|e| anyhow!("building forward message: {e}"))
    }
}

fn parse_mailbox(input: &str) -> Result<Mailbox> {
    input
        .parse::<Mailbox>()
        .with_context(|| format!("parsing mailbox {input:?}"))
}

fn parse_recipients(input: &[String]) -> Result<Vec<Mailbox>> {
    if input.is_empty() {
        anyhow::bail!("forwarder needs at least one recipient");
    }
    input.iter().map(|s| parse_mailbox(s)).collect()
}

/// Filename-safe slug for the forwarded `*-original.eml` attachment.
/// Falls back to `"receipt"` when the subject hint slugifies to empty
/// (no ASCII alphanumerics).
fn attachment_slug(s: &str) -> String {
    let slug = slugify(s, false);
    if slug.is_empty() {
        "receipt".to_string()
    } else {
        slug
    }
}

/// Pull a `Subject:` line out of the raw RFC822 to use as a
/// human-readable hint in the forwarded message's own subject. Falls
/// back to a generic label when none is present.
pub fn subject_hint(raw: &[u8]) -> String {
    let Ok((headers, _)) = mailparse::parse_headers(raw) else {
        return "receipt".to_string();
    };
    let subject = headers
        .iter()
        .find(|h| h.get_key_ref().eq_ignore_ascii_case("subject"))
        .map(|h| h.get_value())
        .unwrap_or_default();
    if subject.trim().is_empty() {
        "receipt".to_string()
    } else {
        subject
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    fn test_handle() -> Handle {
        use std::sync::OnceLock;
        static RT: OnceLock<Runtime> = OnceLock::new();
        RT.get_or_init(|| Runtime::new().unwrap()).handle().clone()
    }

    #[test]
    fn sendmail_builds_message() {
        let fwd = MailForwarder::sendmail(
            "mailsift@example.org",
            vec!["receipts@example.org".into()],
            None,
            test_handle(),
        )
        .unwrap();
        let msg = fwd
            .build_message(b"From: a\r\nSubject: Order #123\r\n\r\nbody", "Order #123")
            .unwrap();
        let wire = String::from_utf8_lossy(&msg.formatted()).into_owned();
        assert!(wire.contains("Subject: [receipt] Order #123"));
        assert!(wire.contains("Content-Type: message/rfc822"));
        assert!(wire.contains("multipart/mixed"));
        // The original message body is included verbatim inside the
        // message/rfc822 attachment.
        assert!(wire.contains("From: a"));
    }

    #[test]
    fn sendmail_rejects_no_recipients() {
        let err = match MailForwarder::sendmail("mailsift@example.org", vec![], None, test_handle())
        {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("at least one recipient"), "{err}");
    }

    #[test]
    fn subject_hint_falls_back_when_missing() {
        assert_eq!(subject_hint(b"From: a\r\n\r\nbody"), "receipt");
    }

    #[test]
    fn subject_hint_reads_header() {
        assert_eq!(
            subject_hint(b"Subject: Your order #ABC123\r\nFrom: a\r\n\r\nbody"),
            "Your order #ABC123"
        );
    }

    #[test]
    fn attachment_slug_handles_punctuation_and_empty() {
        assert_eq!(
            attachment_slug("Order #123 — confirmed"),
            "order-123-confirmed"
        );
        assert_eq!(attachment_slug(""), "receipt");
        assert_eq!(attachment_slug("???"), "receipt");
    }

    #[test]
    #[cfg(feature = "smtp")]
    fn smtp_rejects_password_in_url() {
        let err = match MailForwarder::smtp(
            "mailsift@example.org",
            vec!["receipts@example.org".into()],
            "smtps://user:pw@mail.example.org",
            None,
            test_handle(),
        ) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("password in SMTP URL"), "{err}");
    }
}
