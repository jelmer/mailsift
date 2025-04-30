use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::artifacts::{Artifact, Kind};
use crate::dkim;
use crate::extractor;
use crate::seen::Store as SeenStore;
use crate::stats::{self, Recorder};
use crate::targets::EventSinkKind;

mod router;

const DEFAULT_EXTRACTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the pipeline routes each artifact kind for a single message.
///
/// Borrowed so that the milter / imap-scan paths can hold an owned
/// equivalent ([`OwnedTargets`]) once and produce a fresh borrowed view
/// for each message without cloning the underlying directories.
///
/// Add a new artifact-routing option here and the pipeline picks it up;
/// no other call site needs to touch its argument list.
#[derive(Clone, Copy)]
pub struct PipelineTargets<'a> {
    pub event_sink: &'a EventSinkKind,
    pub bills_dir: Option<&'a Path>,
    pub parcels_dir: Option<&'a Path>,
    /// Directory under which to file `subscription` artifacts. When
    /// `None`, subscription artifacts are dropped with a warning.
    pub subscriptions_dir: Option<&'a Path>,
    /// Receipt target (local directory, WebDAV collection, or mail
    /// forwarder). When `None`, receipt artifacts are dropped with a
    /// warning.
    pub receipts: Option<&'a crate::targets::receipts::ReceiptSink>,
    /// Ticket target (local directory or WebDAV collection). When
    /// `None`, ticket artifacts are dropped with a warning.
    pub tickets: Option<&'a crate::targets::tickets::TicketSink>,
    /// Firefly III sink. When set, every filed bill is also registered
    /// (update-or-create) with Firefly. Failures are best-effort and
    /// don't affect the on-disk record.
    pub firefly: Option<&'a crate::targets::firefly::FireflySink>,
    /// Tracker-registration sinks (Karrio, 17track, ...). When a new
    /// parcel record is created, each configured sink gets the chance
    /// to register the tracking number with its upstream service.
    pub trackers: Option<&'a crate::targets::trackers::Trackers>,
    /// Email addresses we trust to forward vendor mail to us. When the
    /// outer `From:` matches one of these and the mail carries a
    /// `message/rfc822` attachment, the pipeline acts on the inner
    /// message instead; DKIM is rechecked against the inner mail's
    /// own `Authentication-Results` header.
    pub trusted_forwarders: &'a [String],
    /// Per-run event recorder. Each extractor decision (matched /
    /// produced / skipped / failed) is appended to the recorder for
    /// later aggregation by the `stats` subcommand. Pass
    /// [`Recorder::Disabled`] to turn recording off in tests and one-off
    /// CLI runs that shouldn't pollute the long-term log.
    pub recorder: &'a Recorder,
    /// Dedup store. When set, the router checks `(kind, dedup_key,
    /// content_hash)` before re-issuing expensive upstream calls
    /// (currently: CalDAV PUTs of unchanged events). `None` falls
    /// back to the original "always issue, server figures it out"
    /// behaviour; fine for one-off replays and tests.
    pub seen: Option<&'a SeenStore>,
}

/// Long-lived counterpart to [`PipelineTargets`] for callers that hand
/// the targets out across messages (milter, imap-scan). Holds the
/// `event_sink` behind an `Arc` so it can be shared and cloned cheaply.
#[derive(Clone)]
pub struct OwnedTargets {
    pub event_sink: Arc<EventSinkKind>,
    pub bills_dir: Option<PathBuf>,
    pub parcels_dir: Option<PathBuf>,
    pub subscriptions_dir: Option<PathBuf>,
    pub receipts: Option<Arc<crate::targets::receipts::ReceiptSink>>,
    pub tickets: Option<Arc<crate::targets::tickets::TicketSink>>,
    pub firefly: Option<Arc<crate::targets::firefly::FireflySink>>,
    pub trackers: Option<Arc<crate::targets::trackers::Trackers>>,
    /// See [`PipelineTargets::trusted_forwarders`].
    pub trusted_forwarders: Vec<String>,
    /// See [`PipelineTargets::recorder`].
    pub recorder: Recorder,
    /// See [`PipelineTargets::seen`].
    pub seen: Option<SeenStore>,
}

impl OwnedTargets {
    pub fn borrowed(&self) -> PipelineTargets<'_> {
        PipelineTargets {
            event_sink: &self.event_sink,
            bills_dir: self.bills_dir.as_deref(),
            parcels_dir: self.parcels_dir.as_deref(),
            subscriptions_dir: self.subscriptions_dir.as_deref(),
            receipts: self.receipts.as_deref(),
            tickets: self.tickets.as_deref(),
            firefly: self.firefly.as_deref(),
            trackers: self.trackers.as_deref(),
            trusted_forwarders: &self.trusted_forwarders,
            recorder: &self.recorder,
            seen: self.seen.as_ref(),
        }
    }
}

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

/// Per-extractor record emitted while [`run`] iterates the manifest
/// list. Populated only when the caller passes `Some(&mut Vec<_>)` as
/// the `explain` argument; the milter and imap-scan paths leave it
/// `None` and pay nothing for the bookkeeping.
#[derive(Debug)]
pub struct ExplainRecord {
    pub extractor: String,
    pub outcome: ExplainOutcome,
}

#[derive(Debug)]
pub enum ExplainOutcome {
    /// Header prefilter (`from_domains` / `subject_regex`) ruled the
    /// extractor out.
    SkippedHeaders,
    /// `requires:` body shape didn't match.
    SkippedBody,
    /// `require_dkim` wasn't satisfied.
    SkippedDkim,
    /// Extractor was forked but its process failed.
    Failed { error: String },
    /// Extractor produced these per-kind artifact counts. The five
    /// counts mirror the order used in [`router::Summary::render`]:
    /// events, bills, parcels, receipts, tickets.
    Produced {
        events: u32,
        reservations: u32,
        bills: u32,
        parcels: u32,
        receipts: u32,
        tickets: u32,
        subscriptions: u32,
    },
}

/// Run every discovered extractor against `raw` and route the artifacts
/// they emit.
///
/// `event` and `reservation` artifacts go to `event_sink`; `bill`,
/// `parcel`, `receipt`, and `ticket` artifacts go to their respective
/// directories. Each `*_dir` is optional and a missing one drops that
/// artifact kind with a warning.
///
/// `source` is a short label naming where this message came from
/// (e.g. `UID 1234`, `replay foo.eml`, `milter`). It appears in the
/// single rollup INFO line emitted when at least one artifact is
/// successfully filed, so the user can correlate output back to a
/// specific message.
pub fn run(
    raw: &[u8],
    source: &str,
    extractors: &[extractor::Extractor],
    targets: PipelineTargets<'_>,
    dkim_policy: DkimPolicy,
    _dry_run: bool,
    mut explain: Option<&mut Vec<ExplainRecord>>,
) -> Result<()> {
    let PipelineTargets {
        event_sink,
        bills_dir,
        parcels_dir,
        subscriptions_dir,
        receipts,
        tickets,
        firefly,
        trackers,
        trusted_forwarders,
        recorder,
        seen,
    } = targets;

    if extractors.is_empty() {
        warn!("no extractors configured; nothing to do");
        return Ok(());
    }

    // If the message is a forward from a trusted sender, swap `raw`
    // for the inner RFC822 bytes so DKIM checks, prefilter matching,
    // and extractor stdin all see the original vendor mail. The
    // `_unwrapped` binding keeps the inner bytes alive for the rest of
    // `run`.
    let _unwrapped = crate::unforward::try_unwrap_forwarded(raw, trusted_forwarders);
    let raw: &[u8] = match _unwrapped.as_deref() {
        Some(inner) => inner,
        None => raw,
    };

    // Parse headers once and reuse them for everything we need from
    // header land: the DKIM trust check, the manifest prefilter's
    // `From`/`Subject` lookup, and the year-fallback for receipts.
    // If the raw message fails to parse we treat headers as empty;
    // each downstream lookup degrades gracefully.
    let parsed_headers = mailparse::parse_headers(raw)
        .map(|(headers, _)| headers)
        .unwrap_or_default();

    let dkim_domains = match dkim_policy {
        DkimPolicy::Enforce => dkim::passing_dkim_domains(&parsed_headers),
        DkimPolicy::Skip => Default::default(),
    };

    let (from_domain, subject) = parse_match_headers_from_parsed(&parsed_headers);

    // Stamp every recorded event with the same `ts` so a single
    // message's per-extractor lines cluster in the log. SystemTime
    // failures (clock before 1970) collapse to 0.
    let now_ts: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Walk the MIME tree once and build a body-shape summary so each
    // extractor's `requires:` can be checked without forking its
    // Python subprocess. If parsing fails (truncated/garbled message)
    // we skip the body-shape check and let the extractor itself
    // decide.
    let body_parts = mailparse::parse_mail(raw).ok().map(body_parts_from_mail);

    debug!(count = extractors.len(), "running extractors");

    let mut summary = router::Summary::default();

    for ex in extractors {
        if !ex.matches_headers(from_domain.as_deref(), subject.as_deref()) {
            debug!(
                extractor = %ex.name,
                "skipping: from/subject doesn't match extractor's manifest hints"
            );
            record_skip(
                explain.as_deref_mut(),
                recorder,
                ex,
                now_ts,
                from_domain.as_deref(),
                ExplainOutcome::SkippedHeaders,
                stats::Outcome::SkippedHeaders,
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
            record_skip(
                explain.as_deref_mut(),
                recorder,
                ex,
                now_ts,
                from_domain.as_deref(),
                ExplainOutcome::SkippedBody,
                stats::Outcome::SkippedBody,
            );
            continue;
        }

        if !ex.require_dkim.is_empty() {
            match dkim_policy {
                DkimPolicy::Skip => {
                    // No-op: caller has chosen to skip DKIM checks. We
                    // still run the extractor.
                }
                DkimPolicy::Enforce => {
                    if !dkim::satisfies(&ex.require_dkim, &dkim_domains) {
                        debug!(
                            extractor = %ex.name,
                            required = ?ex.require_dkim,
                            "skipping: message lacks a passing DKIM signature from a required domain"
                        );
                        record_skip(
                            explain.as_deref_mut(),
                            recorder,
                            ex,
                            now_ts,
                            from_domain.as_deref(),
                            ExplainOutcome::SkippedDkim,
                            stats::Outcome::SkippedDkim,
                        );
                        continue;
                    }
                }
            }
        }

        debug!(extractor = %ex.name, "running");
        let started = std::time::Instant::now();
        let run = match extractor::run_one(ex, raw, DEFAULT_EXTRACTOR_TIMEOUT) {
            Ok(r) => r,
            Err(e) => {
                let elapsed = started.elapsed();
                warn!(extractor = %ex.name, error = format!("{e:#}"), "extractor failed");
                if let Some(buf) = explain.as_deref_mut() {
                    buf.push(ExplainRecord {
                        extractor: ex.name.clone(),
                        outcome: ExplainOutcome::Failed {
                            error: format!("{e:#}"),
                        },
                    });
                }
                recorder.record(&stats::Event {
                    ts: now_ts,
                    extractor: ex.name.clone(),
                    outcome: stats::Outcome::Failed,
                    duration_ms: Some(stats::duration_to_ms(elapsed)),
                    from_domain: from_domain.clone(),
                });
                continue;
            }
        };
        let elapsed = started.elapsed();

        if let Some(manifest) = &run.result.manifest {
            for note in &manifest.notes {
                debug!(extractor = %run.extractor, note = %note, "extractor note");
            }
        }

        let mut events: Vec<&Artifact> = Vec::new();
        let mut reservations: Vec<&Artifact> = Vec::new();
        let mut bill_arts: Vec<&Artifact> = Vec::new();
        let mut parcel_arts: Vec<&Artifact> = Vec::new();
        let mut receipt_arts: Vec<&Artifact> = Vec::new();
        let mut ticket_arts: Vec<&Artifact> = Vec::new();
        let mut subscription_arts: Vec<&Artifact> = Vec::new();
        for artifact in &run.result.artifacts {
            match artifact.kind {
                Kind::Event => events.push(artifact),
                Kind::Reservation => reservations.push(artifact),
                Kind::Bill => bill_arts.push(artifact),
                Kind::Parcel => parcel_arts.push(artifact),
                Kind::Receipt => receipt_arts.push(artifact),
                Kind::Ticket => ticket_arts.push(artifact),
                Kind::Subscription => subscription_arts.push(artifact),
            }
        }

        let total_artifacts = events.len()
            + reservations.len()
            + bill_arts.len()
            + parcel_arts.len()
            + receipt_arts.len()
            + ticket_arts.len()
            + subscription_arts.len();
        if let Some(buf) = explain.as_deref_mut() {
            buf.push(ExplainRecord {
                extractor: run.extractor.clone(),
                outcome: ExplainOutcome::Produced {
                    events: events.len() as u32,
                    reservations: reservations.len() as u32,
                    bills: bill_arts.len() as u32,
                    parcels: parcel_arts.len() as u32,
                    receipts: receipt_arts.len() as u32,
                    tickets: ticket_arts.len() as u32,
                    subscriptions: subscription_arts.len() as u32,
                },
            });
        }
        recorder.record(&stats::Event {
            ts: now_ts,
            extractor: run.extractor.clone(),
            outcome: if total_artifacts == 0 {
                stats::Outcome::Empty
            } else {
                stats::Outcome::Produced
            },
            duration_ms: Some(stats::duration_to_ms(elapsed)),
            from_domain: from_domain.clone(),
        });

        for artifact in events {
            router::file_event_artifact(&run.extractor, artifact, event_sink, seen, &mut summary);
        }

        for artifact in reservations {
            router::file_reservation_artifact(
                &run.extractor,
                artifact,
                event_sink,
                seen,
                &mut summary,
            );
        }

        file_or_drop(
            "bill",
            &run.extractor,
            bill_arts,
            bills_dir,
            |artifact, dir| {
                router::file_bill_artifact(&run.extractor, artifact, dir, firefly, &mut summary);
            },
        );

        file_or_drop(
            "parcel",
            &run.extractor,
            parcel_arts,
            parcels_dir,
            |artifact, dir| {
                router::file_parcel_artifact(&run.extractor, artifact, dir, trackers, &mut summary);
            },
        );

        file_or_drop(
            "subscription",
            &run.extractor,
            subscription_arts,
            subscriptions_dir,
            |artifact, dir| {
                router::file_subscription_artifact(&run.extractor, artifact, dir, &mut summary);
            },
        );

        file_or_drop(
            "receipt",
            &run.extractor,
            receipt_arts,
            receipts,
            |artifact, sink| {
                router::file_receipt_artifact(&run.extractor, artifact, raw, sink, &mut summary);
            },
        );

        // A ticket on its own is just a binary blob, but it usually
        // ships with a sibling `.event.ics` or `.reservation.json` in
        // the same extractor run; that's where the relevant date
        // lives (DTSTART, departureTime, checkinTime, ...). Use the
        // earliest such date for filing. When no sibling is present
        // we fall back to the message Date and warn: the extractor
        // really should have emitted one.
        let ticket_year = (!ticket_arts.is_empty() && tickets.is_some()).then(|| {
            router::earliest_sibling_year(&run.result.artifacts).unwrap_or_else(|| {
                warn!(
                    extractor = %run.extractor,
                    "ticket emitted without a sibling event/reservation; falling back to message Date"
                );
                message_date_year(&parsed_headers).unwrap_or_else(current_year)
            })
        });
        file_or_drop(
            "ticket",
            &run.extractor,
            ticket_arts,
            tickets,
            |artifact, sink| {
                router::file_ticket_artifact(
                    &run.extractor,
                    artifact,
                    ticket_year.expect("year computed when both tickets and sink exist"),
                    sink,
                    &mut summary,
                );
            },
        );
    }

    if !summary.is_empty() {
        info!("extracted from {source}: {}", summary.render());
    }

    Ok(())
}

/// Same as [`parse_match_headers_from_parsed`] but accepts raw RFC822
/// header bytes, for callers (the IMAP prefilter) that obtain
/// `BODY[HEADER.FIELDS (From Subject)]` from the server rather than
/// having already parsed the full message.
pub fn match_headers_from_raw(raw: &[u8]) -> (Option<String>, Option<String>) {
    let Ok((headers, _)) = mailparse::parse_headers(raw) else {
        return (None, None);
    };
    parse_match_headers_from_parsed(&headers)
}

/// Extract the `From` domain and the `Subject` from already-parsed
/// headers, for the manifest prefilter check. Both are best-effort;
/// either or both may be `None` when the header is missing or
/// unparseable.
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

/// Pull the domain part out of a `From:` header value.
///
/// Handles both `Name <user@example.com>` and bare `user@example.com`.
/// Returns `None` if no `@` is present.
fn parse_from_domain(value: &str) -> Option<String> {
    // Take the content between the last `<` and matching `>` if
    // present; otherwise treat the whole value as the address.
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
/// extractor `requires:` check consumes. Mirrors the IMAP-prefilter
/// walker in [`crate::imap_scan`], so a message can be filtered the
/// same way regardless of which path it arrived on.
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
        // back to the legacy Content-Type `name=` parameter. Case is
        // already normalised by mailparse; keys are lowercased and
        // quotes stripped.
        let disp = mail.get_content_disposition();
        let filename = disp
            .params
            .get("filename")
            .or_else(|| mail.ctype.params.get("name"))
            .map(String::as_str);
        out.push_leaf(ty, subtype, filename);
    }

    // multipart containers don't themselves count as a leaf; only
    // their children do. mailparse exposes message/rfc822 as a leaf
    // with no subparts; the forwarded-mail case is handled upstream
    // by `try_unwrap_forwarded`, which substitutes the inner bytes
    // before we get here.
    for child in &mail.subparts {
        collect_parts(child, out);
    }
}

/// Read a year from the message's `Date:` header. Best-effort; returns
/// `None` if the header is missing or unparseable.
fn message_date_year(headers: &[mailparse::MailHeader<'_>]) -> Option<i32> {
    use chrono::Datelike;
    let date_value = headers
        .iter()
        .find(|h| h.get_key_ref().eq_ignore_ascii_case("date"))?
        .get_value();
    let dt = mailparse::dateparse(&date_value).ok()?;
    let dt = chrono::DateTime::from_timestamp(dt, 0)?;
    Some(dt.year())
}

fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Utc::now().year()
}

/// Push a skip outcome to both the `explain` buffer (when set) and
/// the stats recorder. Factored out because the three skip sites
/// (`SkippedHeaders` / `SkippedBody` / `SkippedDkim`) would otherwise
/// each repeat the same six-line block.
fn record_skip(
    explain: Option<&mut Vec<ExplainRecord>>,
    recorder: &Recorder,
    ex: &extractor::Extractor,
    ts: i64,
    from_domain: Option<&str>,
    explain_outcome: ExplainOutcome,
    stats_outcome: stats::Outcome,
) {
    if let Some(buf) = explain {
        buf.push(ExplainRecord {
            extractor: ex.name.clone(),
            outcome: explain_outcome,
        });
    }
    recorder.record(&stats::Event {
        ts,
        extractor: ex.name.clone(),
        outcome: stats_outcome,
        duration_ms: None,
        from_domain: from_domain.map(str::to_string),
    });
}

/// File every artifact in `artifacts` via `file_each`. When the
/// sink/dir is `None`, emit a single WARN summarising what got
/// dropped instead, to avoid the per-artifact spam from messages
/// that produce many artifacts of a kind without a configured target.
fn file_or_drop<S: Copy>(
    kind: &'static str,
    extractor: &str,
    artifacts: Vec<&Artifact>,
    sink: Option<S>,
    mut file_each: impl FnMut(&Artifact, S),
) {
    if artifacts.is_empty() {
        return;
    }
    let Some(sink) = sink else {
        let count = artifacts.len();
        let noun = if count == 1 {
            kind.to_string()
        } else {
            format!("{kind}s")
        };
        warn!(
            extractor,
            "no {kind} target configured; dropping {count} {noun}"
        );
        return;
    };
    for artifact in artifacts {
        file_each(artifact, sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_parse_from_domain_bare_address() {
        assert_eq!(
            parse_from_domain("user@Example.com"),
            Some("example.com".into())
        );
    }

    #[test]
    fn pipeline_parse_from_domain_name_with_angles() {
        assert_eq!(
            parse_from_domain("Alice <alice@example.com>"),
            Some("example.com".into())
        );
    }

    #[test]
    fn pipeline_parse_from_domain_returns_none_without_at() {
        assert_eq!(parse_from_domain("not-an-address"), None);
    }

    #[test]
    fn pipeline_parse_from_domain_returns_none_for_empty_after_at() {
        assert_eq!(parse_from_domain("user@"), None);
    }

    #[test]
    fn pipeline_parse_from_domain_handles_inverted_angles() {
        // Pathological `>...<`; the code falls back to treating the
        // whole value as an address. Verifies the `start < end` guard
        // doesn't slice with a bad range.
        assert_eq!(
            parse_from_domain(">x<a@example.com"),
            Some("example.com".into())
        );
    }

    #[test]
    fn match_headers_extracts_from_domain_and_subject() {
        let raw = b"From: Alice <alice@Example.COM>\r\nSubject: Hello there\r\n\r\n";
        let (from, subj) = match_headers_from_raw(raw);
        assert_eq!(from.as_deref(), Some("example.com"));
        assert_eq!(subj.as_deref(), Some("Hello there"));
    }

    #[test]
    fn match_headers_handles_missing_headers() {
        let raw = b"To: nobody@example.com\r\n\r\n";
        let (from, subj) = match_headers_from_raw(raw);
        assert_eq!(from, None);
        assert_eq!(subj, None);
    }

    #[test]
    fn match_headers_returns_subject_without_from() {
        let raw = b"Subject: Lonely\r\n\r\n";
        let (from, subj) = match_headers_from_raw(raw);
        assert_eq!(from, None);
        assert_eq!(subj.as_deref(), Some("Lonely"));
    }

    fn parsed_headers(raw: &[u8]) -> Vec<mailparse::MailHeader<'_>> {
        let (headers, _) = mailparse::parse_headers(raw).unwrap();
        headers
    }

    #[test]
    fn message_date_year_reads_year() {
        let raw = b"Date: Sat, 27 Jun 2026 12:00:00 +0000\r\n\r\n";
        let h = parsed_headers(raw);
        assert_eq!(message_date_year(&h), Some(2026));
    }

    #[test]
    fn message_date_year_returns_none_when_missing() {
        let raw = b"From: x\r\n\r\n";
        let h = parsed_headers(raw);
        assert_eq!(message_date_year(&h), None);
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
        assert!(parts.attachment_filenames.is_empty());
    }

    #[test]
    fn body_parts_multipart_alternative() {
        let raw = b"From: x\r\n\
Content-Type: multipart/alternative; boundary=BOUND\r\n\
\r\n\
--BOUND\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
plain body\r\n\
--BOUND\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>html body</p>\r\n\
--BOUND--\r\n";
        let parts = parts_from(raw);
        assert!(parts.has_text);
        assert!(parts.has_html);
        assert_eq!(parts.mime_types.len(), 2);
    }

    #[test]
    fn body_parts_picks_up_calendar_attachment_with_disposition_filename() {
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

    #[test]
    fn body_parts_falls_back_to_content_type_name() {
        let raw = b"From: x\r\n\
Content-Type: application/pdf; name=\"invoice.pdf\"\r\n\
\r\n\
%PDF-1.4\r\n";
        let parts = parts_from(raw);
        assert_eq!(parts.attachment_filenames, vec!["invoice.pdf"]);
    }
}
