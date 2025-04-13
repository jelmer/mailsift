use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::artifacts::{Artifact, Kind};
use crate::dkim;
use crate::extractor;
use crate::targets::{EventSink, EventSinkKind, FileOutcome, split_calendar};

/// Whether to enforce `require_dkim` constraints declared in extractor
/// manifests.
#[derive(Clone, Copy, Debug)]
pub enum DkimPolicy {
    /// Honour each extractor's `require_dkim`. Used for replay,
    /// imap-scan, and maildir-watch; modes where the message has
    /// already passed through our MTA's DKIM check and the topmost
    /// `Authentication-Results` header is trustworthy.
    Enforce,
    /// Skip DKIM checks entirely. Used by the milter front-end, which
    /// sees mail before our MTA has authenticated it.
    Skip,
}

const DEFAULT_EXTRACTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Run every discovered extractor against `raw` and route the artifacts
/// they emit. `source` is a short label naming where the message came
/// from (e.g. `replay foo.eml`); it appears in the rollup INFO line so
/// the user can correlate output back to a specific message.
pub fn run(
    raw: &[u8],
    source: &str,
    extractors_dir: &Path,
    event_sink: &EventSinkKind,
    trusted_forwarders: &[String],
    dkim_policy: DkimPolicy,
    _dry_run: bool,
) -> Result<()> {
    let extractors = extractor::discover(extractors_dir)
        .with_context(|| format!("discovering extractors in {}", extractors_dir.display()))?;
    if extractors.is_empty() {
        warn!("no extractors configured; nothing to do");
        return Ok(());
    }

    // If the message is a forward from a trusted sender, swap `raw`
    // for the inner RFC822 bytes so prefilter matching and extractor
    // stdin all see the original vendor mail. The `_unwrapped`
    // binding keeps the inner bytes alive for the rest of `run`.
    let _unwrapped = crate::unforward::try_unwrap_forwarded(raw, trusted_forwarders);
    let raw: &[u8] = match _unwrapped.as_deref() {
        Some(inner) => inner,
        None => raw,
    };

    // Parse headers once for the prefilter; if parsing fails we treat
    // them as empty and let each downstream check degrade gracefully.
    let parsed_headers = mailparse::parse_headers(raw)
        .map(|(headers, _)| headers)
        .unwrap_or_default();
    let (from_domain, subject) = parse_match_headers_from_parsed(&parsed_headers);

    let dkim_domains = match dkim_policy {
        DkimPolicy::Enforce => dkim::passing_dkim_domains(&parsed_headers),
        DkimPolicy::Skip => Default::default(),
    };

    // Walk the MIME tree once and build a body-shape summary so each
    // extractor's `requires:` can be checked without forking its
    // subprocess. If parsing fails (truncated/garbled message) we skip
    // the body-shape check and let the extractor itself decide.
    let body_parts = mailparse::parse_mail(raw).ok().map(body_parts_from_mail);

    debug!(count = extractors.len(), "running extractors");

    let mut filed = 0usize;

    for ex in &extractors {
        if !ex.matches_headers(from_domain.as_deref(), subject.as_deref()) {
            debug!(
                extractor = %ex.name,
                "skipping: from/subject doesn't match extractor's manifest hints"
            );
            continue;
        }

        if let Some(parts) = &body_parts
            && !ex.body_could_match(parts)
        {
            debug!(
                extractor = %ex.name,
                "skipping: message body shape doesn't satisfy extractor's `requires:`"
            );
            continue;
        }

        if !ex.require_dkim.is_empty()
            && matches!(dkim_policy, DkimPolicy::Enforce)
            && !dkim::satisfies(&ex.require_dkim, &dkim_domains)
        {
            debug!(
                extractor = %ex.name,
                required = ?ex.require_dkim,
                "skipping: message lacks a passing DKIM signature from a required domain"
            );
            continue;
        }

        debug!(extractor = %ex.name, "running");
        let run = match extractor::run_one(ex, raw, DEFAULT_EXTRACTOR_TIMEOUT) {
            Ok(r) => r,
            Err(e) => {
                warn!(extractor = %ex.name, error = format!("{e:#}"), "extractor failed");
                continue;
            }
        };

        for artifact in &run.result.artifacts {
            match artifact.kind {
                Kind::Event => {
                    if file_event_artifact(&run.extractor, artifact, event_sink) {
                        filed += 1;
                    }
                }
                Kind::Reservation => {
                    if file_reservation_artifact(&run.extractor, artifact, event_sink) {
                        filed += 1;
                    }
                }
            }
        }
    }

    if filed > 0 {
        info!("extracted from {source}: {filed} event(s)");
    }

    Ok(())
}

/// File a `reservation` artifact: parse the schema.org JSON, convert
/// to a single VEVENT, and file via the event sink.
fn file_reservation_artifact(
    extractor: &str,
    artifact: &Artifact,
    event_sink: &EventSinkKind,
) -> bool {
    let events = match crate::reservation::convert_file(&artifact.path) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to convert reservation JSON"
            );
            return false;
        }
    };
    if events.is_empty() {
        warn!(
            extractor,
            path = %artifact.path.display(),
            "reservation JSON yielded no events (unknown @type or missing fields)"
        );
        return false;
    }
    let mut any_filed = false;
    for event in &events {
        match event_sink.file(event) {
            Ok(FileOutcome::Created(label)) => {
                info!(extractor, uid = %event.uid, target = %label, "reservation filed");
                any_filed = true;
            }
            Ok(FileOutcome::Updated(label)) => {
                info!(extractor, uid = %event.uid, target = %label, "reservation updated");
                any_filed = true;
            }
            Err(e) => {
                warn!(
                    extractor,
                    uid = %event.uid,
                    error = format!("{e:#}"),
                    "failed to file reservation"
                );
            }
        }
    }
    any_filed
}

fn file_event_artifact(extractor: &str, artifact: &Artifact, event_sink: &EventSinkKind) -> bool {
    let body = match fs::read_to_string(&artifact.path) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to read event body"
            );
            return false;
        }
    };
    let events = match split_calendar(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                extractor,
                path = %artifact.path.display(),
                error = format!("{e:#}"),
                "failed to parse event body"
            );
            return false;
        }
    };
    if events.is_empty() {
        warn!(
            extractor,
            path = %artifact.path.display(),
            "event body parsed but yielded no VEVENT components",
        );
        return false;
    }
    let mut any_filed = false;
    for event in &events {
        match event_sink.file(event) {
            Ok(FileOutcome::Created(label)) => {
                info!(extractor, uid = %event.uid, target = %label, "event filed");
                any_filed = true;
            }
            Ok(FileOutcome::Updated(label)) => {
                info!(extractor, uid = %event.uid, target = %label, "event updated");
                any_filed = true;
            }
            Err(e) => {
                warn!(
                    extractor,
                    uid = %event.uid,
                    error = format!("{e:#}"),
                    "failed to file event"
                );
            }
        }
    }
    any_filed
}

/// Extract the `From` domain and the `Subject` from already-parsed
/// headers. Both are best-effort.
fn parse_match_headers_from_parsed(
    headers: &[mailparse::MailHeader<'_>],
) -> (Option<String>, Option<String>) {
    let from_value = headers
        .iter()
        .find(|h| h.get_key_ref().eq_ignore_ascii_case("from"))
        .map(|h| h.get_value());
    let from_domain = from_value.as_deref().and_then(parse_from_domain);
    let subject = headers
        .iter()
        .find(|h| h.get_key_ref().eq_ignore_ascii_case("subject"))
        .map(|h| h.get_value());
    (from_domain, subject)
}

/// Pull the domain part out of a `From:` header value. Handles
/// `Name <user@example.com>` and bare `user@example.com`. Returns
/// `None` if no `@` is present.
fn parse_from_domain(value: &str) -> Option<String> {
    let addr = if let (Some(start), Some(end)) = (value.rfind('<'), value.rfind('>')) {
        if start < end {
            &value[start + 1..end]
        } else {
            value
        }
    } else {
        value
    };
    let after_at = addr.rsplit_once('@')?.1.trim();
    if after_at.is_empty() {
        None
    } else {
        Some(after_at.to_ascii_lowercase())
    }
}

/// Flatten a parsed MIME tree into the [`BodyParts`] summary the
/// extractor `requires:` check consumes.
fn body_parts_from_mail(mail: mailparse::ParsedMail<'_>) -> extractor::BodyParts {
    let mut parts = extractor::BodyParts::default();
    collect_parts(&mail, &mut parts);
    parts
}

fn collect_parts(mail: &mailparse::ParsedMail<'_>, out: &mut extractor::BodyParts) {
    let mimetype = mail.ctype.mimetype.to_ascii_lowercase();
    let (ty, subtype) = mimetype.split_once('/').unwrap_or((mimetype.as_str(), ""));
    let is_multipart = ty == "multipart";

    if !is_multipart {
        // Prefer Content-Disposition's `filename=` (RFC 2183); fall
        // back to the legacy Content-Type `name=` parameter.
        let disp = mail.get_content_disposition();
        let filename = disp
            .params
            .get("filename")
            .or_else(|| mail.ctype.params.get("name"))
            .map(String::as_str);
        out.push_leaf(ty, subtype, filename);
    }

    for child in &mail.subparts {
        collect_parts(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_from_domain_bare_address() {
        assert_eq!(
            parse_from_domain("user@Example.com"),
            Some("example.com".into())
        );
    }

    #[test]
    fn parse_from_domain_name_with_angles() {
        assert_eq!(
            parse_from_domain("Alice <alice@example.com>"),
            Some("example.com".into())
        );
    }

    #[test]
    fn parse_from_domain_returns_none_without_at() {
        assert_eq!(parse_from_domain("not-an-address"), None);
    }

    fn parts_from(raw: &[u8]) -> extractor::BodyParts {
        let mail = mailparse::parse_mail(raw).expect("parse_mail");
        body_parts_from_mail(mail)
    }

    #[test]
    fn body_parts_single_text_plain() {
        let raw = b"From: x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n";
        let parts = parts_from(raw);
        assert!(parts.has_text);
        assert!(!parts.has_html);
        assert_eq!(parts.mime_types, vec![("text".into(), "plain".into())]);
    }

    #[test]
    fn body_parts_picks_up_calendar_attachment() {
        let raw = b"From: x\r\n\
Content-Type: multipart/mixed; boundary=BOUND\r\n\
\r\n\
--BOUND\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
plain\r\n\
--BOUND\r\n\
Content-Type: text/calendar; charset=utf-8\r\n\
Content-Disposition: attachment; filename=\"invite.ics\"\r\n\
\r\n\
BEGIN:VCALENDAR\r\n\
END:VCALENDAR\r\n\
--BOUND--\r\n";
        let parts = parts_from(raw);
        assert!(parts.has_text);
        assert!(
            parts
                .mime_types
                .iter()
                .any(|(t, s)| t == "text" && s == "calendar")
        );
        assert_eq!(parts.attachment_filenames, vec!["invite.ics"]);
    }
}
