//! `mailsift imap-scan`: walk an IMAP mailbox and run the pipeline
//! over each message.
//!
//! Useful for batch-processing a backlog without going through the
//! milter or shuffling files into a Maildir. Read-only: we never set
//! flags, expunge, or move messages.
//!
//! With `--watch`, after the initial scan the same session stays
//! connected and uses `IDLE` ([RFC 2177]) to be notified of new mail;
//! each notification triggers a UID search for everything past the
//! cursor and processes any new messages. Runs until interrupted; on
//! transport errors the connection is rebuilt with exponential backoff.
//!
//! [RFC 2177]: https://tools.ietf.org/html/rfc2177

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use imap::Session;
use imap::extensions::idle::WaitOutcome;
use imap::types::UnsolicitedResponse;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::extractor::BodyParts;
use crate::pipeline::{self, PipelineTargets};

/// How many UIDs we group into a single `UID FETCH` round trip. Bigger
/// is fewer round trips but more memory pressure (each message's
/// RFC822 body is buffered in full before we start processing). 50 is
/// a comfortable midpoint for typical mail sizes.
const FETCH_BATCH: usize = 50;

/// How to authenticate to the IMAP server.
#[derive(Clone, Copy)]
pub enum AuthMethod<'a> {
    /// IMAP `LOGIN` with user + password.
    Login { user: &'a str, password: &'a str },
    /// SASL `AUTHENTICATE GSSAPI` using credentials from the caller's
    /// Kerberos credential cache. `authzid` is the optional SASL
    /// authorization identity.
    #[cfg(feature = "gssapi")]
    Gssapi { authzid: Option<&'a str> },
    /// SASL `AUTHENTICATE XOAUTH2` with a Gmail (or other provider)
    /// OAuth2 bearer token. The token is short-lived; obtain a fresh
    /// one before each run (e.g. via `oauth2l` or `gcloud`).
    XOAuth2 {
        user: &'a str,
        access_token: &'a str,
    },
}

/// SASL `XOAUTH2` authenticator. Builds the single client message per
/// [Google's XOAUTH2 spec][1]: `user=<email>\x01auth=Bearer <token>\x01\x01`.
/// The server sends an empty challenge first; on auth failure it sends
/// a base64 JSON error blob followed by `*` to abort, which the `imap`
/// crate surfaces as a normal `BAD` response.
///
/// [1]: https://developers.google.com/gmail/imap/xoauth2-protocol
struct XOAuth2Authenticator {
    user: String,
    access_token: String,
}

impl imap::Authenticator for XOAuth2Authenticator {
    type Response = String;

    fn process(&self, _challenge: &[u8]) -> Self::Response {
        format!(
            "user={}\x01auth=Bearer {}\x01\x01",
            self.user, self.access_token
        )
    }
}

pub struct ImapScanConfig<'a> {
    pub host: &'a str,
    pub port: u16,
    pub auth: AuthMethod<'a>,
    pub mailbox: &'a str,
    pub since: Option<&'a str>,
    pub limit: Option<usize>,
    pub extractors: &'a [crate::extractor::Extractor],
    pub targets: PipelineTargets<'a>,
    pub dry_run: bool,
    /// After the initial scan, stay connected and use IMAP `IDLE`
    /// (RFC 2177) to be notified of new mail. Each notification triggers
    /// a UID search for everything past the highest UID processed so far
    /// and runs the pipeline over any new messages. Loops until
    /// interrupted (SIGINT/SIGTERM); on transport errors the connection
    /// is rebuilt with exponential backoff.
    pub watch: bool,
}

pub fn run(config: ImapScanConfig<'_>) -> Result<()> {
    // Process-wide interrupt flag for --watch. Installed once; if the
    // installer fails (e.g. another handler already bound) we log and
    // carry on; the IDLE keepalive will still keep the loop alive,
    // just won't quit cleanly on Ctrl-C.
    let interrupted = Arc::new(AtomicBool::new(false));
    if config.watch {
        let flag = Arc::clone(&interrupted);
        if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
            warn!(error = %e, "could not install SIGINT handler; Ctrl-C may not exit cleanly");
        }
    }

    let mut session = connect_and_authenticate(&config)?;
    let mbox = session
        .examine(config.mailbox)
        .with_context(|| format!("EXAMINE {}", config.mailbox))?;
    info!(
        mailbox = config.mailbox,
        exists = mbox.exists,
        "selected (read-only)"
    );

    let query = build_search_query(config.since);
    let mut uids: Vec<u32> = session
        .uid_search(&query)
        .with_context(|| format!("UID SEARCH {query}"))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    info!(matched = uids.len(), "UIDs returned by search");

    let take = match config.limit {
        Some(n) => uids.len().min(n),
        None => uids.len(),
    };
    let initial_uids = &uids[..take];

    // Cursor for --watch: highest UID we've already considered.
    // Initialised from the EXAMINE response so messages that appeared
    // between SEARCH and the first IDLE aren't skipped; we re-search
    // from `cursor+1` on every wakeup, and uid_validity ensures the
    // cursor is meaningful (if it changes we bail rather than chase a
    // renumbered mailbox).
    let mut cursor = initial_uids.iter().copied().max().unwrap_or(0);
    let uid_validity = mbox.uid_validity;

    let pb = make_progress_bar(initial_uids.len() as u64);
    let stats = process_uid_set(&mut session, initial_uids, &config, &pb)?;
    pb.finish_and_clear();
    if stats.prefilter_skipped > 0 {
        info!(
            skipped = stats.prefilter_skipped,
            fetched = stats.processed,
            "prefilter skipped body fetches"
        );
    }

    if !config.watch {
        session.logout().context("IMAP LOGOUT")?;
        return Ok(());
    }

    info!(
        mailbox = config.mailbox,
        cursor, "entering watch mode (IDLE)"
    );
    watch_loop(session, &mut cursor, uid_validity, &config, &interrupted)
}

/// Open a fresh connection and authenticate. Used by both the initial
/// call and the post-disconnect reconnect path in [`watch_loop`].
fn connect_and_authenticate(config: &ImapScanConfig<'_>) -> Result<Session<imap::Connection>> {
    let client = imap::ClientBuilder::new(config.host, config.port)
        .connect()
        .with_context(|| format!("connecting to {}:{}", config.host, config.port))?;

    let session = match config.auth {
        AuthMethod::Login { user, password } => client
            .login(user, password)
            .map_err(|(e, _)| e)
            .context("IMAP LOGIN")?,
        #[cfg(feature = "gssapi")]
        AuthMethod::Gssapi { authzid } => {
            let authenticator = imap::gssapi::GssapiAuthenticator::new(
                "imap",
                config.host,
                authzid.map(str::to_string),
            )
            .context("initialising GSSAPI client context")?;
            client
                .authenticate("GSSAPI", &authenticator)
                .map_err(|(e, _)| {
                    if let Some(detail) = authenticator.last_error() {
                        anyhow::anyhow!("IMAP AUTHENTICATE GSSAPI: {e} ({detail})")
                    } else {
                        anyhow::anyhow!("IMAP AUTHENTICATE GSSAPI: {e}")
                    }
                })?
        }
        AuthMethod::XOAuth2 { user, access_token } => {
            let authenticator = XOAuth2Authenticator {
                user: user.to_string(),
                access_token: access_token.to_string(),
            };
            client
                .authenticate("XOAUTH2", &authenticator)
                .map_err(|(e, _)| anyhow::anyhow!("IMAP AUTHENTICATE XOAUTH2: {e}"))?
        }
    };

    info!(
        host = config.host,
        mailbox = config.mailbox,
        "IMAP authentication OK"
    );
    Ok(session)
}

/// Build the standard progress bar. Auto-hides when stderr isn't a TTY
/// (output piped or redirected to a log file). Steady-tick redraws the
/// bar every 200 ms even when no `pb.inc`/`pb.set_message` is called;
/// `tracing` log lines write to stderr without going through indicatif
/// and visually overwrite the bar; the steady tick brings it back into
/// view rather than leaving the line blank until the next message
/// finishes.
fn make_progress_bar(len: u64) -> ProgressBar {
    let pb = ProgressBar::new(len).with_style(
        ProgressStyle::with_template("{spinner} [{elapsed_precise}] [{bar:40}] {pos}/{len} {msg}")
            .expect("static template is valid")
            .progress_chars("=> "),
    );
    pb.enable_steady_tick(Duration::from_millis(200));
    pb
}

#[derive(Default)]
struct ScanStats {
    processed: usize,
    prefilter_skipped: usize,
}

/// Walk `uids` in `FETCH_BATCH`-sized chunks, prefilter via
/// `BODYSTRUCTURE` + headers, fetch RFC822 for survivors, run the
/// pipeline over each body in parallel.
///
/// Batches the FETCH round trips: one network round trip per
/// `FETCH_BATCH` messages instead of one per message. We still process
/// each returned message serially; parallelising extraction is a
/// separate change. Each batch starts with a cheap pre-pass that asks
/// IMAP for the `From`/`Subject` headers plus `BODYSTRUCTURE`, so we
/// can decide whether any extractor's `from_domains`, `subject_regex`,
/// and `requires:` hints could match without paying for the full
/// RFC822 body. Catch-all extractors with no header hints still benefit
/// when they declare body `requires:` (e.g. `ics-passthrough` only
/// wants messages with a `text/calendar` part).
fn process_uid_set(
    session: &mut Session<imap::Connection>,
    uids: &[u32],
    config: &ImapScanConfig<'_>,
    pb: &ProgressBar,
) -> Result<ScanStats> {
    let mut stats = ScanStats::default();
    for chunk in uids.chunks(FETCH_BATCH) {
        pb.set_message(format!(
            "UIDs {}..={}",
            chunk.first().copied().unwrap_or(0),
            chunk.last().copied().unwrap_or(0),
        ));

        let prefilter_set = uid_set(chunk);
        let prefilter_fetched = session
            .uid_fetch(
                &prefilter_set,
                "(BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)] BODYSTRUCTURE)",
            )
            .with_context(|| format!("UID FETCH {prefilter_set} prefilter"))?;

        let mut body_set: Vec<u32> = Vec::with_capacity(chunk.len());
        for message in prefilter_fetched.iter() {
            let Some(uid) = message.uid else {
                warn!("prefilter FETCH response without UID");
                continue;
            };
            let (from_domain, subject) = match message.header() {
                Some(raw) => pipeline::match_headers_from_raw(raw),
                None => (None, None),
            };
            // No BODYSTRUCTURE; fall back to header-only filtering.
            // Shouldn't happen for a well-formed response but isn't
            // fatal; we'd rather fetch the body and have the extractor
            // decide than silently skip a real message.
            let parts = message.bodystructure().map(body_parts_from_structure);
            let any_match = config.extractors.iter().any(|e| {
                if !e.matches_headers(from_domain.as_deref(), subject.as_deref()) {
                    return false;
                }
                match &parts {
                    Some(p) => e.body_could_match(p),
                    None => true,
                }
            });
            if any_match {
                body_set.push(uid);
            } else {
                stats.prefilter_skipped += 1;
                pb.inc(1);
            }
        }
        body_set.sort_unstable();

        if body_set.is_empty() {
            continue;
        }

        let set = uid_set(&body_set);
        let fetched = session
            .uid_fetch(&set, "RFC822")
            .with_context(|| format!("UID FETCH {set}"))?;

        // Copy each message's body out of the IMAP buffer so we can
        // drop the borrow on `fetched` (and the session) and feed the
        // bodies to a worker pool. Each extractor run forks a Python
        // subprocess, so the bottleneck is the OS scheduler, not Rust
        // CPU; `rayon`'s default thread count works well here.
        let mut messages: Vec<(u32, Vec<u8>)> = Vec::with_capacity(body_set.len());
        for message in fetched.iter() {
            let Some(uid) = message.uid else {
                warn!("FETCH response without UID");
                continue;
            };
            let Some(body) = message.body() else {
                warn!(uid, "message has no RFC822 body");
                pb.inc(1);
                continue;
            };
            messages.push((uid, body.to_vec()));
        }
        drop(fetched);

        stats.processed += messages.len();

        messages.par_iter().for_each(|(uid, body)| {
            debug!(uid, size = body.len(), "processing");
            let source = format!("UID {uid}");
            let result = pipeline::run(
                body,
                &source,
                config.extractors,
                config.targets,
                pipeline::DkimPolicy::Enforce,
                config.dry_run,
                None,
            );
            if let Err(e) = result {
                warn!(uid = *uid, error = %e, "pipeline failed");
            }
            pb.inc(1);
        });
    }
    Ok(stats)
}

/// Maximum backoff between failed reconnect attempts. Capped so a
/// long-running watch eventually retries every minute regardless of
/// how many failures have accumulated, without DDoSing the server.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// IDLE keepalive interval. RFC 2177 says servers may log out clients
/// after 29 minutes; we DONE+IDLE more frequently so we (a) stay well
/// under that limit, (b) get a chance to check the interrupt flag, and
/// (c) catch missed events on servers that occasionally drop
/// notifications (this has been observed on Dovecot under load; a
/// safety net rescan on every wakeup costs little).
const IDLE_KEEPALIVE: Duration = Duration::from_secs(5 * 60);

/// Watch loop: IDLE → check for new UIDs → process → repeat.
///
/// On transport errors we drop the session and rebuild with exponential
/// backoff (1, 2, 4, ..., 60s). On UIDVALIDITY change we bail loudly
/// rather than silently chase renumbered mail.
fn watch_loop(
    mut session: Session<imap::Connection>,
    cursor: &mut u32,
    mut uid_validity: Option<u32>,
    config: &ImapScanConfig<'_>,
    interrupted: &Arc<AtomicBool>,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        if interrupted.load(Ordering::SeqCst) {
            info!("interrupted, leaving watch mode");
            let _ = session.logout();
            return Ok(());
        }

        // IDLE until the server tells us something changed, our
        // keepalive fires, or the underlying socket dies.
        let wait_result = {
            let mut handle = session.idle();
            handle.timeout(IDLE_KEEPALIVE).keepalive(false);
            handle.wait_while(|response| {
                // Any EXISTS / RECENT means new mail; bail out and
                // re-search. Anything else (e.g. FETCH flag updates
                // from another client) we ignore and keep idling.
                !matches!(response, UnsolicitedResponse::Exists(_))
            })
        };

        match wait_result {
            Ok(WaitOutcome::MailboxChanged) | Ok(WaitOutcome::TimedOut) => {}
            Err(e) => {
                warn!(error = %e, "IDLE failed; reconnecting");
                session = match reconnect(config, &mut backoff, interrupted) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                if let Some(new_validity) = reselect(&mut session, config, uid_validity)? {
                    uid_validity = Some(new_validity);
                }
                continue;
            }
        }

        // Whether the IDLE wake was a real EXISTS or just our keepalive
        // tick, ask the server for everything past the cursor. Doing
        // this on every wakeup also rescues us from servers that
        // occasionally swallow notifications.
        let new_uids = match search_after(&mut session, *cursor) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "UID SEARCH after cursor failed; reconnecting");
                session = match reconnect(config, &mut backoff, interrupted) {
                    Some(s) => s,
                    None => return Ok(()),
                };
                if let Some(new_validity) = reselect(&mut session, config, uid_validity)? {
                    uid_validity = Some(new_validity);
                }
                continue;
            }
        };

        // A successful round trip means the connection is healthy;
        // reset the backoff so the next failure starts at 1 s again.
        backoff = Duration::from_secs(1);

        if new_uids.is_empty() {
            continue;
        }
        info!(count = new_uids.len(), "new messages while watching");
        let pb = make_progress_bar(new_uids.len() as u64);
        let stats = process_uid_set(&mut session, &new_uids, config, &pb)?;
        pb.finish_and_clear();
        if stats.prefilter_skipped > 0 {
            info!(
                skipped = stats.prefilter_skipped,
                fetched = stats.processed,
                "prefilter skipped body fetches"
            );
        }
        if let Some(max) = new_uids.iter().copied().max() {
            *cursor = max;
        }
    }
}

/// Return UIDs strictly greater than `cursor`. Uses IMAP's `UID N:*`
/// search syntax; `*` matches the highest assigned UID, so a quiet
/// mailbox returns an empty set.
///
/// The set may briefly include `cursor` itself if the server's `*`
/// quirk resolves to it; we strip that explicitly so the watch loop
/// never re-processes the cursor message.
fn search_after(session: &mut Session<imap::Connection>, cursor: u32) -> Result<Vec<u32>> {
    let query = search_after_query(cursor);
    let mut v: Vec<u32> = session
        .uid_search(&query)
        .with_context(|| format!("UID SEARCH {query}"))?
        .into_iter()
        .filter(|&uid| uid > cursor)
        .collect();
    v.sort_unstable();
    Ok(v)
}

/// Build the `UID N:*` search query for [`search_after`]. Extracted so
/// the format is unit-testable without a live IMAP connection.
fn search_after_query(cursor: u32) -> String {
    format!("UID {}:*", cursor.saturating_add(1))
}

/// Reconnect with exponential backoff up to [`RECONNECT_BACKOFF_MAX`].
/// Honours the interrupt flag during sleeps; Ctrl-C while waiting to
/// retry exits cleanly. Returns `None` if interrupted.
fn reconnect(
    config: &ImapScanConfig<'_>,
    backoff: &mut Duration,
    interrupted: &Arc<AtomicBool>,
) -> Option<Session<imap::Connection>> {
    loop {
        if interrupted.load(Ordering::SeqCst) {
            return None;
        }
        warn!(?backoff, "reconnect attempt");
        std::thread::sleep(*backoff);
        if interrupted.load(Ordering::SeqCst) {
            return None;
        }
        match connect_and_authenticate(config) {
            Ok(s) => {
                info!("reconnected");
                return Some(s);
            }
            Err(e) => {
                warn!(error = %e, "reconnect failed");
                *backoff = (*backoff * 2).min(RECONNECT_BACKOFF_MAX);
            }
        }
    }
}

/// Re-EXAMINE the mailbox after a reconnect. Returns the new
/// `uid_validity` so the caller can compare against the previous one.
/// Bails if `uid_validity` changed; that means the server renumbered
/// the mailbox (rare; usually only after a restore from backup) and
/// the cursor is no longer meaningful.
fn reselect(
    session: &mut Session<imap::Connection>,
    config: &ImapScanConfig<'_>,
    previous: Option<u32>,
) -> Result<Option<u32>> {
    let mbox = session
        .examine(config.mailbox)
        .with_context(|| format!("EXAMINE {} after reconnect", config.mailbox))?;
    if let (Some(prev), Some(now)) = (previous, mbox.uid_validity)
        && prev != now
    {
        anyhow::bail!(
            "UIDVALIDITY changed ({prev} -> {now}); the mailbox was renumbered, refusing to continue silently"
        );
    }
    Ok(mbox.uid_validity)
}

fn build_search_query(since: Option<&str>) -> String {
    match since {
        Some(s) => format!("SINCE {s}"),
        None => "ALL".to_string(),
    }
}

/// Flatten an IMAP `BODYSTRUCTURE` response into the [`BodyParts`]
/// summary the extractor prefilter consumes. Walks `Multipart` and
/// `Message` containers and records every leaf part.
fn body_parts_from_structure(bs: &imap_proto::BodyStructure<'_>) -> BodyParts {
    let mut parts = BodyParts::default();
    collect_parts(bs, &mut parts);
    parts
}

fn collect_parts(bs: &imap_proto::BodyStructure<'_>, out: &mut BodyParts) {
    use imap_proto::types::{BodyContentCommon, BodyStructure};

    fn leaf(common: &BodyContentCommon<'_>, out: &mut BodyParts) {
        let ty = common.ty.ty.to_ascii_lowercase();
        let subtype = common.ty.subtype.to_ascii_lowercase();

        // Prefer Content-Disposition's `filename=` (RFC 2183); fall
        // back to the legacy Content-Type `name=` parameter. Param
        // keys are case-insensitive per RFC 2045.
        let filename = common
            .disposition
            .as_ref()
            .and_then(|d| d.params.as_ref())
            .and_then(|ps| {
                ps.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("filename"))
                    .map(|(_, v)| v.to_string())
            })
            .or_else(|| {
                common.ty.params.as_ref().and_then(|ps| {
                    ps.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("name"))
                        .map(|(_, v)| v.to_string())
                })
            });

        out.push_leaf(&ty, &subtype, filename.as_deref());
    }

    match bs {
        BodyStructure::Basic { common, .. } | BodyStructure::Text { common, .. } => {
            leaf(common, out);
        }
        BodyStructure::Message { common, body, .. } => {
            // RFC822 message attachment; surface it as a leaf so
            // `attachment:message/rfc822` style requirements can match,
            // and also descend, so html/text/calendar parts inside it
            // count.
            leaf(common, out);
            collect_parts(body, out);
        }
        BodyStructure::Multipart { bodies, .. } => {
            for child in bodies {
                collect_parts(child, out);
            }
        }
    }
}

/// Render a sorted slice of UIDs as an IMAP sequence set, compressing
/// consecutive runs into `a:b` ranges.
///
/// Sequence-set syntax (RFC 3501 §9): the request `UID FETCH 1,3:5,7`
/// is equivalent to `UID FETCH 1,3,4,5,7` but uses far fewer bytes for
/// long, dense mailboxes. The input must be sorted ascending; callers
/// in this module already sort.
fn uid_set(sorted: &[u32]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let mut i = 0;
    while i < sorted.len() {
        if !out.is_empty() {
            out.push(',');
        }
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        if start == end {
            let _ = write!(out, "{start}");
        } else {
            let _ = write!(out, "{start}:{end}");
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_query_default() {
        assert_eq!(build_search_query(None), "ALL");
    }

    #[test]
    fn search_query_since() {
        assert_eq!(build_search_query(Some("01-Jan-2026")), "SINCE 01-Jan-2026");
    }

    #[test]
    fn uid_set_empty() {
        assert_eq!(uid_set(&[]), "");
    }

    #[test]
    fn uid_set_singleton() {
        assert_eq!(uid_set(&[42]), "42");
    }

    #[test]
    fn uid_set_scattered() {
        assert_eq!(uid_set(&[1, 5, 9]), "1,5,9");
    }

    #[test]
    fn uid_set_one_run() {
        assert_eq!(uid_set(&[1, 2, 3, 4, 5]), "1:5");
    }

    #[test]
    fn uid_set_mixed() {
        assert_eq!(uid_set(&[1, 2, 3, 5, 7, 8, 10]), "1:3,5,7:8,10");
    }

    #[test]
    fn search_after_query_from_zero() {
        // Initial watch on an empty (or not-yet-scanned) mailbox.
        // `UID 1:*` is the standard "everything that exists" form.
        assert_eq!(search_after_query(0), "UID 1:*");
    }

    #[test]
    fn search_after_query_from_nonzero_cursor() {
        // Typical watch tick after some UIDs are already processed.
        assert_eq!(search_after_query(42), "UID 43:*");
    }

    #[test]
    fn search_after_query_saturates_at_u32_max() {
        // Defensive: u32::MAX as cursor would overflow naively. The
        // resulting query is silly (UID MAX:*) but the function must
        // not panic; the server will return an empty set.
        assert_eq!(search_after_query(u32::MAX), format!("UID {}:*", u32::MAX));
    }

    #[test]
    fn xoauth2_client_response_format() {
        use imap::Authenticator;
        let auth = XOAuth2Authenticator {
            user: "someone@example.com".to_string(),
            access_token: "ya29.a0AfH6SMB".to_string(),
        };
        let response = auth.process(b"");
        assert_eq!(
            response,
            "user=someone@example.com\x01auth=Bearer ya29.a0AfH6SMB\x01\x01"
        );
    }

    #[test]
    fn xoauth2_ignores_server_challenge_payload() {
        // The server can send a base64 error payload as a challenge if
        // the token was rejected, but the client's response in that
        // case is still just the same SASL message; the imap crate then
        // surfaces the failure via the tagged BAD/NO response. So
        // `process` must not vary with the challenge bytes.
        use imap::Authenticator;
        let auth = XOAuth2Authenticator {
            user: "u@example.com".to_string(),
            access_token: "tok".to_string(),
        };
        assert_eq!(auth.process(b""), auth.process(b"some-error-blob"));
    }

    /// Parse a single `* N FETCH (...)` line and extract the
    /// `BODYSTRUCTURE` attribute, then walk it. Lets us write tests
    /// against real IMAP wire format rather than constructing
    /// `BodyStructure` values by hand.
    fn parts_from_fetch_line(line: &[u8]) -> BodyParts {
        let (_, response) = imap_proto::parser::parse_response(line).expect("parse FETCH");
        let imap_proto::Response::Fetch(_, attrs) = response else {
            panic!("expected Fetch response, got {response:?}");
        };
        for attr in &attrs {
            if let imap_proto::AttributeValue::BodyStructure(bs) = attr {
                return body_parts_from_structure(bs);
            }
        }
        panic!("FETCH had no BODYSTRUCTURE attribute");
    }

    #[test]
    fn body_parts_single_text_plain() {
        // `* 1 FETCH (BODYSTRUCTURE ("text" "plain" ("charset" "utf-8") NIL NIL "7bit" 12 1))`
        let parts = parts_from_fetch_line(
            b"* 1 FETCH (BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 12 1))\r\n",
        );
        assert!(parts.has_text);
        assert!(!parts.has_html);
        assert_eq!(parts.mime_types, vec![("text".into(), "plain".into())]);
        assert!(parts.attachment_filenames.is_empty());
    }

    #[test]
    fn body_parts_multipart_alternative_html_and_text() {
        // text/plain + text/html, classic alternative.
        let parts = parts_from_fetch_line(
            b"* 1 FETCH (BODYSTRUCTURE ((\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 12 1)(\"text\" \"html\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 34 1) \"alternative\"))\r\n",
        );
        assert!(parts.has_text);
        assert!(parts.has_html);
        assert_eq!(parts.mime_types.len(), 2);
    }

    #[test]
    fn body_parts_picks_up_calendar_attachment() {
        // multipart/mixed wrapping a text/plain and a text/calendar
        // attachment named invite.ics. Disposition carries the
        // filename.
        let parts = parts_from_fetch_line(
            b"* 1 FETCH (BODYSTRUCTURE ((\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 12 1)(\"text\" \"calendar\" (\"charset\" \"utf-8\" \"name\" \"invite.ics\") NIL NIL \"7bit\" 100 5) \"mixed\"))\r\n",
        );
        assert!(parts.has_text);
        assert!(
            parts
                .mime_types
                .iter()
                .any(|(t, s)| t == "text" && s == "calendar")
        );
        assert!(parts.attachment_filenames.iter().any(|f| f == "invite.ics"));
    }
}
