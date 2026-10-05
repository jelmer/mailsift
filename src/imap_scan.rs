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

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// The mailbox named on the command line does not exist on the server. A
/// caller mistake rather than an internal failure, so the CLI reports it
/// as a plain message without a backtrace.
#[derive(Debug)]
pub struct MailboxNotFound {
    pub mailbox: String,
}

impl std::fmt::Display for MailboxNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no such mailbox: {}", self.mailbox)
    }
}

impl std::error::Error for MailboxNotFound {}

/// How to authenticate to the IMAP server.
pub enum AuthMethod<'a> {
    /// IMAP `LOGIN` with user + password.
    Login { user: &'a str, password: &'a str },
    /// SASL `AUTHENTICATE GSSAPI` using credentials from the caller's
    /// Kerberos credential cache. `authzid` is the optional SASL
    /// authorization identity.
    #[cfg(feature = "gssapi")]
    Gssapi { authzid: Option<&'a str> },
    /// SASL `AUTHENTICATE XOAUTH2`. A fresh access token is fetched from
    /// `tokens` at every (re)connect. With a
    /// [`StaticTokenProvider`](crate::oauth2::StaticTokenProvider) that
    /// is a fixed pre-obtained token (fine for a run that finishes within
    /// its lifetime); with a
    /// [`TokenProvider`](crate::oauth2::TokenProvider) it is minted from a
    /// refresh token, so it survives token expiry across a `--watch`
    /// session's reconnects.
    XOAuth2 {
        user: &'a str,
        tokens: &'a dyn crate::oauth2::TokenSource,
    },
}

/// SASL `XOAUTH2` authenticator. Builds the single client message per
/// [Google's XOAUTH2 spec][1]: `user=<email>\x01auth=Bearer <token>\x01\x01`.
/// The server sends an empty challenge first; on auth failure it sends
/// a base64 JSON error blob followed by `*` to abort, which the `imap`
/// crate surfaces as a normal `BAD` response.
///
/// [1]: https://developers.google.com/gmail/imap/xoauth2-protocol
struct XOAuth2Authenticator<'a> {
    user: &'a str,
    access_token: &'a str,
}

impl imap::Authenticator for XOAuth2Authenticator<'_> {
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
    /// Upper bound on message internal date, exclusive, in IMAP date
    /// format. Combined with `since` this bounds a scan to a specific
    /// window so a single IMAP session doesn't outrun the server's
    /// idle timeout on very large mailboxes.
    pub before: Option<&'a str>,
    pub limit: Option<usize>,
    pub extractors: &'a [crate::extractor::Extractor],
    /// Directories the extractors were loaded from. Non-empty under
    /// `--watch` enables the extractor filesystem watcher: any change
    /// under one of these directories triggers a reload; extractors
    /// whose fingerprint changed (or that appeared) are re-run against
    /// the same UID range as the initial scan, so a manifest edit or a
    /// new extractor picks up historical mail without a restart.
    ///
    /// An empty slice means "the caller has fixed the extractor set,
    /// don't watch." `main.rs` passes empty when `--only` was given,
    /// so a mid-flight fs event can't silently broaden the set past
    /// what the user asked for; the field is empty rather than
    /// `Option<_>` because "no watching" is the natural degenerate
    /// case of "these dirs."
    pub extractor_dirs: &'a [PathBuf],
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
    let mbox = match session.examine(config.mailbox) {
        Ok(mbox) => mbox,
        // A NO response to EXAMINE means the server refused to select the
        // mailbox; for a mailbox the user named, that is almost always
        // "it doesn't exist". Surface it as such rather than a backtrace.
        Err(imap::Error::No(_)) => {
            return Err(MailboxNotFound {
                mailbox: config.mailbox.to_string(),
            }
            .into());
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("EXAMINE {}", config.mailbox)));
        }
    };
    info!(
        mailbox = config.mailbox,
        exists = mbox.exists,
        "selected (read-only)"
    );

    let base_query = build_search_query(config.since, config.before);
    let (uids, from_restriction) =
        narrowed_uid_search(&mut session, &base_query, config.extractors)?;
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
    let stats = if config.watch {
        // Under --watch, a fetch failure mid-scan is treated the same as
        // one inside the watch loop: back off, reconnect, resume from the
        // next unprocessed chunk. Without this, a mid-scan disconnect
        // (server drop, TLS close_notify race) would exit the process,
        // and while systemd will restart us the next scan hits the same
        // large initial backlog again.
        process_uids_with_resume(
            &mut session,
            initial_uids,
            &config,
            config.extractors,
            &pb,
            uid_validity,
            &interrupted,
        )?
    } else {
        process_uid_set(&mut session, initial_uids, &config, config.extractors, &pb)?
    };
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

    if interrupted.load(Ordering::SeqCst) {
        let _ = session.logout();
        return Ok(());
    }

    info!(
        mailbox = config.mailbox,
        cursor, "entering watch mode (IDLE)"
    );
    watch_loop(
        session,
        &mut cursor,
        uid_validity,
        &config,
        from_restriction.as_deref(),
        base_query.as_str(),
        &interrupted,
    )
}

/// Wrap [`process_uid_set`] with the same fetch-backoff / reconnect /
/// reselect story that [`watch_loop`] uses. Chunk-level errors during the
/// initial scan drop the current session, sleep the current fetch backoff,
/// reconnect, re-EXAMINE, and resume from the next unprocessed chunk.
///
/// Only used under `--watch`: a one-shot scan surfaces errors so the
/// caller sees them. Under `--watch` we can't afford to exit here because
/// systemd would then restart the process and repeat the same expensive
/// initial SEARCH + prefilter over the whole backlog.
fn process_uids_with_resume(
    session_slot: &mut Session<imap::Connection>,
    uids: &[u32],
    config: &ImapScanConfig<'_>,
    extractors: &[crate::extractor::Extractor],
    pb: &ProgressBar,
    initial_uid_validity: Option<u32>,
    interrupted: &Arc<AtomicBool>,
) -> Result<ScanStats> {
    let mut total = ScanStats::default();
    let mut done = 0usize;
    let mut fetch_backoff = Duration::from_secs(1);
    let mut reconnect_backoff = Duration::from_secs(1);
    let mut uid_validity = initial_uid_validity;
    while done < uids.len() {
        if interrupted.load(Ordering::SeqCst) {
            return Ok(total);
        }
        let before = done;
        let result = process_uid_set_with_progress(
            session_slot,
            &uids[done..],
            config,
            extractors,
            pb,
            &mut done,
        );
        match result {
            Ok(stats) => {
                total.processed += stats.processed;
                total.prefilter_skipped += stats.prefilter_skipped;
                return Ok(total);
            }
            Err(e) => {
                // Any forward progress means the connection was healthy
                // right up to the failing chunk; only a chunk that fails
                // on the very first attempt after a reconnect charges
                // against the backoff.
                if done > before {
                    fetch_backoff = Duration::from_secs(1);
                }
                warn!(
                    error = %e,
                    done,
                    total = uids.len(),
                    ?fetch_backoff,
                    "initial scan fetch failed; backing off and reconnecting"
                );
                std::thread::sleep(fetch_backoff);
                if interrupted.load(Ordering::SeqCst) {
                    return Ok(total);
                }
                fetch_backoff = grow_backoff(fetch_backoff, FETCH_BACKOFF_MAX);
                *session_slot = match reconnect(config, &mut reconnect_backoff, interrupted) {
                    Some(s) => s,
                    None => return Ok(total),
                };
                if let Some(new_validity) = reselect(session_slot, config, uid_validity)? {
                    uid_validity = Some(new_validity);
                }
            }
        }
    }
    Ok(total)
}

/// Open a fresh connection and authenticate. Used by both the initial
/// call and the post-disconnect reconnect path in [`watch_loop`].
fn connect_and_authenticate(config: &ImapScanConfig<'_>) -> Result<Session<imap::Connection>> {
    let client = imap::ClientBuilder::new(config.host, config.port)
        .connect()
        .with_context(|| format!("connecting to {}:{}", config.host, config.port))?;

    let session = match &config.auth {
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
        AuthMethod::XOAuth2 { user, tokens } => {
            // Fetch (or reuse a cached) access token now, so a reconnect
            // after the previous token expired picks up a fresh one.
            let access_token = tokens.access_token()?;
            let authenticator = XOAuth2Authenticator {
                user,
                access_token: &access_token,
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
    extractors: &[crate::extractor::Extractor],
    pb: &ProgressBar,
) -> Result<ScanStats> {
    let mut done = 0usize;
    process_uid_set_with_progress(session, uids, config, extractors, pb, &mut done)
}

/// Body of [`process_uid_set`] with an out parameter for how many UIDs
/// have been fully consumed. The callers in one-shot mode discard it;
/// [`process_uids_with_resume`] reads it after a failure so it can resume
/// from the next chunk on the next connection.
fn process_uid_set_with_progress(
    session: &mut Session<imap::Connection>,
    uids: &[u32],
    config: &ImapScanConfig<'_>,
    extractors: &[crate::extractor::Extractor],
    pb: &ProgressBar,
    done: &mut usize,
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
            let any_match = extractors.iter().any(|e| {
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
                extractors,
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
        // A chunk is only counted as done once every UID in it has been
        // fully processed (either prefiltered out or fetched and pushed
        // through the pipeline). If we return with an error, `*done`
        // still points at the first UID of the failing chunk, so a
        // resuming caller retries that chunk on the fresh connection.
        *done += chunk.len();
    }
    Ok(stats)
}

/// Maximum backoff between failed reconnect attempts. Capped so a
/// long-running watch eventually retries every minute regardless of
/// how many failures have accumulated, without DDoSing the server.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Maximum backoff after a failed fetch. Unlike a failed connect, this
/// can mean the server is rate-limiting us, which on Gmail lasts hours
/// rather than seconds; retrying every minute would keep us in the
/// penalty box, so this backs off much further.
const FETCH_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

/// Double a backoff, capped at `max`. Extracted so the progression is
/// unit-testable without a live connection.
fn grow_backoff(current: Duration, max: Duration) -> Duration {
    (current * 2).min(max)
}

/// IDLE keepalive interval. RFC 2177 says servers may log out clients
/// after 29 minutes; we DONE+IDLE more frequently so we (a) stay well
/// under that limit, (b) get a chance to check the interrupt flag, and
/// (c) catch missed events on servers that occasionally drop
/// notifications (this has been observed on Dovecot under load; a
/// safety net rescan on every wakeup costs little).
const IDLE_KEEPALIVE: Duration = Duration::from_secs(5 * 60);

/// Cap on IDLE wait when the extractor filesystem watcher is armed.
/// A workaround, not a knob: the blocking IDLE API can't be woken by
/// a `notify` event, so the shortest we ever park in IDLE is also the
/// worst-case latency between an extractor edit and its retrigger.
/// Ten seconds is a soft compromise between IMAP round-trip cost and
/// perceived responsiveness.
const EXTRACTOR_WATCH_POLL_INTERVAL: Duration = Duration::from_secs(10);

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
    // Sender restriction to append to each cursor search, or `None`
    // when the selected extractors don't allow narrowing (or the
    // server rejected it during the initial scan).
    from_restriction: Option<&str>,
    // The `--since`/`--before` window from the initial scan. Reused
    // when an extractor change triggers a scoped rescan.
    base_query: &str,
    interrupted: &Arc<AtomicBool>,
) -> Result<()> {
    let mut extractor_watch = ExtractorWatch::start(config)?;

    let mut backoff = Duration::from_secs(1);
    // Tracked separately from `backoff`: a throttled server still
    // accepts connections, so only a successful fetch clears this.
    let mut fetch_backoff = Duration::from_secs(1);
    loop {
        if interrupted.load(Ordering::SeqCst) {
            info!("interrupted, leaving watch mode");
            let _ = session.logout();
            return Ok(());
        }

        // Poll the extractor filesystem watcher between IDLE calls.
        // `poll` returns `Some(changed)` only when the debounce quiet
        // period has elapsed since the last event, so an editor's
        // multi-event save reloads once. Errors during the rescan
        // don't stop the loop -- the next real edit will retry.
        if let Some(watch) = extractor_watch.as_mut()
            && let Some(changed) = watch.poll()
            && let Err(e) = run_scoped_rescan(
                &mut session,
                config,
                &changed,
                base_query,
                uid_validity,
                interrupted,
            )
        {
            warn!(error = %e, "extractor retrigger failed; will retry on next change");
        }

        // IDLE until the server tells us something changed, our
        // keepalive fires, or the underlying socket dies. With the
        // extractor watcher armed, shorten the wait so a manifest edit
        // is picked up within seconds rather than waiting for the
        // full 5-minute keepalive; the fs watcher itself has no way
        // to break IDLE, so we poll it on wake.
        let idle_timeout = extractor_watch
            .as_ref()
            .map_or(IDLE_KEEPALIVE, |_| EXTRACTOR_WATCH_POLL_INTERVAL);
        let wait_result = {
            let mut handle = session.idle();
            handle.timeout(idle_timeout).keepalive(false);
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
        let new_uids = match search_after(&mut session, *cursor, from_restriction) {
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
        // Re-borrow the working extractor set: the poll above may
        // have swapped it out.
        let extractors = extractor_watch
            .as_ref()
            .map_or(config.extractors, |w| w.current.as_slice());
        let fetch_result = process_uid_set(&mut session, &new_uids, config, extractors, &pb);
        pb.finish_and_clear();
        let stats = match fetch_result {
            Ok(stats) => stats,
            // A fetch can die mid-batch when the server throttles us or
            // drops the connection. Treat it like the IDLE and SEARCH
            // failures above: back off and rebuild the session rather
            // than exiting, which would leave systemd restarting us at
            // a flat rate and burning its start limit. The cursor is
            // left alone so the same UIDs are retried after reconnect.
            Err(e) => {
                warn!(
                    error = %e,
                    ?fetch_backoff,
                    "fetching new messages failed; backing off and reconnecting"
                );
                // Reconnecting usually succeeds even while the server
                // is throttling fetches, so `reconnect`'s own backoff
                // never grows. Sleep here instead, and keep growing it
                // until a fetch actually works.
                std::thread::sleep(fetch_backoff);
                if interrupted.load(Ordering::SeqCst) {
                    let _ = session.logout();
                    return Ok(());
                }
                fetch_backoff = grow_backoff(fetch_backoff, FETCH_BACKOFF_MAX);
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
        // Fetches are working again, so start over from 1 s.
        fetch_backoff = Duration::from_secs(1);
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

/// Length of the "quiet period" after the last filesystem event
/// before we consider the storm over and reload. `notify` fires
/// several events for a single "save" from many editors (write, rename,
/// chmod, ...); coalescing them keeps us from reloading three times in
/// a row.
const EXTRACTOR_RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);

/// Filesystem watch on the extractor directories, plus the current
/// working set of loaded extractors and their fingerprints. Only set
/// up under `--watch` when `extractor_dirs` is non-empty; when
/// [`ExtractorWatch::start`] returns `None` the watch loop just uses
/// `config.extractors` as it always did.
struct ExtractorWatch {
    /// Extractors currently in use by the watch loop. Reloaded from
    /// disk when the notify watcher fires; a mutation here changes
    /// which extractors the IDLE-mail path runs.
    current: Vec<crate::extractor::Extractor>,
    /// name -> fingerprint at last successful load. Compared against a
    /// fresh discovery to identify which extractors are new or changed
    /// and need to be re-run against the initial scan window.
    fingerprints: std::collections::HashMap<String, [u8; 32]>,
    /// Directories the extractors were discovered from. Passed back
    /// into `extractor::discover` on reload.
    dirs: Vec<PathBuf>,
    /// Channel drained on each loop iteration. Notify's own thread
    /// pushes events here; we don't care what the event is, only
    /// whether at least one arrived.
    events: mpsc::Receiver<notify::Result<notify::Event>>,
    /// Deadline at which the debounce quiet period elapses. `Some`
    /// only while events have arrived but the reload hasn't fired
    /// yet; each new event pushes it back by
    /// [`EXTRACTOR_RELOAD_DEBOUNCE`]. Read non-blockingly by
    /// [`poll`](Self::poll); never involves a sleep.
    reload_at: Option<Instant>,
    /// Kept alive so the notify thread keeps running; dropping it
    /// stops the watch.
    _watcher: notify::RecommendedWatcher,
}

impl ExtractorWatch {
    fn start(config: &ImapScanConfig<'_>) -> Result<Option<Self>> {
        if config.extractor_dirs.is_empty() {
            return Ok(None);
        }
        // The initial `current` set is a fresh load from disk rather
        // than the caller's `config.extractors` because we need to own
        // it (the caller's slice has an unrelated lifetime) and the
        // caller has already logged which extractors it selected.
        // Any mismatch between the two would show up as a spurious
        // "changed" set on the very first reload, which we don't want.
        let current = crate::extractor::discover(config.extractor_dirs)
            .context("initial extractor discovery for watch")?;
        let fingerprints = fingerprint_map(&current);
        let (watcher, events) = spawn_fs_watcher(config.extractor_dirs)?;
        info!(
            dirs = ?config.extractor_dirs,
            "watching extractor directories for changes"
        );
        Ok(Some(ExtractorWatch {
            current,
            fingerprints,
            dirs: config.extractor_dirs.to_vec(),
            events,
            reload_at: None,
            _watcher: watcher,
        }))
    }

    /// Non-blocking check for a pending extractor reload. Drains any
    /// filesystem events (bumping the debounce deadline forward for
    /// each), and if the deadline has elapsed, reloads from disk and
    /// returns the extractors whose fingerprint changed (or that are
    /// new). `None` means "nothing to do right now" -- either no
    /// events pending, the debounce hasn't elapsed yet, or reload
    /// happened but nothing meaningfully changed.
    ///
    /// Callers are responsible for running the scoped rescan against
    /// the returned subset; the watch's `current`/`fingerprints` have
    /// already been updated by the time this returns.
    fn poll(&mut self) -> Option<Vec<crate::extractor::Extractor>> {
        if self.drain_events() {
            self.reload_at = Some(Instant::now() + EXTRACTOR_RELOAD_DEBOUNCE);
        }
        if !should_reload(Instant::now(), self.reload_at) {
            return None;
        }
        self.reload_at = None;

        let reloaded = match crate::extractor::discover(&self.dirs) {
            Ok(v) => v,
            Err(e) => {
                // A broken manifest during editing is expected; leave
                // `current` as-is so the IDLE mail path keeps working
                // and try again on the next fs event.
                warn!(error = %e, "reload after extractor change failed; keeping previous set");
                return None;
            }
        };

        let changed = diff_reloaded(&mut self.fingerprints, &reloaded);
        self.current = reloaded;

        if changed.is_empty() {
            info!("extractors reloaded; no fingerprint changes");
            return None;
        }
        info!(
            count = changed.len(),
            names = %changed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(","),
            "extractor change detected; retriggering against initial scan window"
        );
        Some(changed)
    }

    /// Non-blockingly drain every pending fs event. Returns `true`
    /// when at least one arrived (so the caller can extend the
    /// debounce deadline). A disconnected channel is logged once and
    /// then reported as "no events" -- the watch thread has died but
    /// there's no way to recover it here.
    fn drain_events(&mut self) -> bool {
        let mut drained_any = false;
        loop {
            match self.events.try_recv() {
                Ok(_) => drained_any = true,
                Err(mpsc::TryRecvError::Empty) => return drained_any,
                Err(mpsc::TryRecvError::Disconnected) => {
                    warn!("extractor filesystem watcher disconnected");
                    return drained_any;
                }
            }
        }
    }
}

/// Should [`ExtractorWatch::poll`] reload right now, given the debounce
/// deadline? `None` means "no events pending"; `Some(t)` means
/// "reload after `t`." Pure, so testable without wall-clock time.
fn should_reload(now: Instant, reload_at: Option<Instant>) -> bool {
    reload_at.is_some_and(|t| now >= t)
}

/// Compare the reloaded extractor set against `fingerprints`,
/// returning the subset whose fingerprint changed (or that appeared
/// for the first time). Updates `fingerprints` in place to match the
/// reloaded set, so a subsequent call with the same input returns
/// nothing.
///
/// A missing entry is either new or renamed; both count as changed
/// because their manifest could match messages the previous set
/// wouldn't have run against. Removed extractors just drop out of
/// `fingerprints`; no rescan is needed.
fn diff_reloaded(
    fingerprints: &mut std::collections::HashMap<String, [u8; 32]>,
    reloaded: &[crate::extractor::Extractor],
) -> Vec<crate::extractor::Extractor> {
    let changed: Vec<_> = reloaded
        .iter()
        .filter(|e| match fingerprints.get(&e.name) {
            Some(prev) => *prev != e.fingerprint(),
            None => true,
        })
        .cloned()
        .collect();
    *fingerprints = fingerprint_map(reloaded);
    changed
}

/// name -> fingerprint index over an extractor set.
fn fingerprint_map(
    extractors: &[crate::extractor::Extractor],
) -> std::collections::HashMap<String, [u8; 32]> {
    extractors
        .iter()
        .map(|e| (e.name.clone(), e.fingerprint()))
        .collect()
}

/// Set up a `notify` recursive watch across `dirs`, returning the
/// watcher (kept alive by the caller so its background thread keeps
/// running) and a receiver for its events. Contents of the events
/// don't matter to us; only whether at least one arrived.
fn spawn_fs_watcher(
    dirs: &[PathBuf],
) -> Result<(
    notify::RecommendedWatcher,
    mpsc::Receiver<notify::Result<notify::Event>>,
)> {
    use notify::Watcher;
    let (tx, rx) = mpsc::channel();
    let mut watcher: notify::RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            // A send failure just means the watch loop has exited
            // and dropped the receiver; nothing to log about.
            let _ = tx.send(res);
        })
        .context("initialising extractor filesystem watcher")?;
    for dir in dirs {
        watcher
            .watch(dir, notify::RecursiveMode::Recursive)
            .with_context(|| format!("watching extractor dir {}", dir.display()))?;
    }
    Ok((watcher, rx))
}

/// Run one bounded rescan of the initial `--since`/`--before` window
/// with `extractors` as the working set. Existing dedup skips
/// artifacts that would be re-emitted identically, so a no-op change
/// doesn't spam the sinks. Used by the extractor watcher after
/// [`ExtractorWatch::poll`] reports a fingerprint change.
fn run_scoped_rescan(
    session: &mut Session<imap::Connection>,
    config: &ImapScanConfig<'_>,
    extractors: &[crate::extractor::Extractor],
    base_query: &str,
    uid_validity: Option<u32>,
    interrupted: &Arc<AtomicBool>,
) -> Result<()> {
    let (uids, _) = narrowed_uid_search(session, base_query, extractors)?;
    if uids.is_empty() {
        info!("rescan window is empty; nothing to retrigger");
        return Ok(());
    }
    info!(
        matched = uids.len(),
        "UIDs to re-process for changed extractors"
    );

    let pb = make_progress_bar(uids.len() as u64);
    let stats = process_uids_with_resume(
        session,
        &uids,
        config,
        extractors,
        &pb,
        uid_validity,
        interrupted,
    )?;
    pb.finish_and_clear();
    if stats.prefilter_skipped > 0 {
        info!(
            skipped = stats.prefilter_skipped,
            fetched = stats.processed,
            "rescan prefilter skipped body fetches"
        );
    }
    Ok(())
}

/// Return UIDs strictly greater than `cursor`. Uses IMAP's `UID N:*`
/// search syntax; `*` matches the highest assigned UID, so a quiet
/// mailbox returns an empty set.
///
/// The set may briefly include `cursor` itself if the server's `*`
/// quirk resolves to it; we strip that explicitly so the watch loop
/// never re-processes the cursor message.
fn search_after(
    session: &mut Session<imap::Connection>,
    cursor: u32,
    from_restriction: Option<&str>,
) -> Result<Vec<u32>> {
    let query = search_after_query(cursor, from_restriction);
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
fn search_after_query(cursor: u32, from_restriction: Option<&str>) -> String {
    let range = format!("UID {}:*", cursor.saturating_add(1));
    match from_restriction {
        Some(r) => format!("{range} {r}"),
        None => range,
    }
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
                *backoff = grow_backoff(*backoff, RECONNECT_BACKOFF_MAX);
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

/// Run one `UID SEARCH` narrowed by the given extractors' sender
/// hints, falling back to the unnarrowed `base_query` if the server
/// disliked the narrowed form (some servers reject deeply-nested `OR`
/// chains). Returns the sorted UIDs and the restriction that actually
/// worked -- `None` when narrowing was skipped (no hints available) or
/// dropped after fallback, so the caller doesn't retry it on later
/// searches within the same session.
///
/// Used by both the initial scan and the extractor-change rescan;
/// the shape of the narrow-then-fallback is the same, only the caller
/// context differs.
fn narrowed_uid_search(
    session: &mut Session<imap::Connection>,
    base_query: &str,
    extractors: &[crate::extractor::Extractor],
) -> Result<(Vec<u32>, Option<String>)> {
    // When every selected extractor names the senders it cares about,
    // let the server drop everything else: no UID for a message we'd
    // only discard in the prefilter. Correctness never rests on this,
    // so a server that rejects the query just costs us the speedup.
    let from_restriction = build_from_restriction(extractors);
    let query = match &from_restriction {
        Some(r) => format!("{base_query} {r}"),
        None => base_query.to_string(),
    };
    let (mut uids, effective_restriction): (Vec<u32>, Option<String>) = match session
        .uid_search(&query)
    {
        Ok(found) => (found.into_iter().collect(), from_restriction),
        // Only a NO/BAD tagged response means the server disliked the
        // query itself. Anything else is a transport failure, which
        // retrying with a different query wouldn't fix and shouldn't
        // hide.
        Err(imap::Error::No(_) | imap::Error::Bad(_)) if from_restriction.is_some() => {
            warn!(
                query,
                "narrowed UID SEARCH rejected by the server; falling back to the unnarrowed query"
            );
            let v: Vec<u32> = session
                .uid_search(base_query)
                .with_context(|| format!("UID SEARCH {base_query}"))?
                .into_iter()
                .collect();
            (v, None)
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("UID SEARCH {query}")));
        }
    };
    uids.sort_unstable();
    Ok((uids, effective_restriction))
}

fn build_search_query(since: Option<&str>, before: Option<&str>) -> String {
    match (since, before) {
        (Some(s), Some(b)) => format!("SINCE {s} BEFORE {b}"),
        (Some(s), None) => format!("SINCE {s}"),
        (None, Some(b)) => format!("BEFORE {b}"),
        (None, None) => "ALL".to_string(),
    }
}

/// The sender restriction to append to a `UID SEARCH`, built from the
/// selected extractors' `from_domains`.
///
/// `None` means "no restriction is safe": some extractor declares no
/// `from_domains` at all and so could match a message from any sender.
/// Otherwise the result is an `OR`-chain of `HEADER FROM` terms.
///
/// IMAP's `OR` is strictly binary (RFC 3501 s6.4.4), so N terms become
/// N-1 prefix `OR`s: `OR OR a b c`. The terms are a deliberate
/// superset of what [`Extractor::matches_headers`] accepts -- a
/// substring test against the whole `From` header, so it also matches
/// display names and lookalike domains -- which is fine because the
/// per-message prefilter still applies the exact rule afterwards.
///
/// [`Extractor::matches_headers`]: crate::extractor::Extractor::matches_headers
fn build_from_restriction(extractors: &[crate::extractor::Extractor]) -> Option<String> {
    if extractors.is_empty() {
        return None;
    }
    let mut domains: Vec<&str> = Vec::new();
    for ex in extractors {
        let roots = ex.from_domain_roots();
        if roots.is_empty() {
            // A hint-less extractor matches any sender; narrowing the
            // search would hide messages it wants.
            return None;
        }
        domains.extend(roots);
    }
    domains.sort_unstable();
    domains.dedup();

    let mut terms = domains
        .iter()
        .map(|d| format!("HEADER FROM {}", quote_imap_string(d)));
    let mut query = terms.next()?;
    for term in terms {
        query = format!("OR {query} {term}");
    }
    Some(query)
}

/// Render `s` as an IMAP quoted string (RFC 3501 s4.3): wrapped in
/// double quotes with `\` and `"` backslash-escaped.
fn quote_imap_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
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
                    .map(|(_, v)| v.as_ref())
            })
            .or_else(|| {
                common.ty.params.as_ref().and_then(|ps| {
                    ps.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("name"))
                        .map(|(_, v)| v.as_ref())
                })
            });

        out.push_leaf(&ty, &subtype, filename);
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
        assert_eq!(build_search_query(None, None), "ALL");
    }

    #[test]
    fn search_query_since() {
        assert_eq!(
            build_search_query(Some("01-Jan-2026"), None),
            "SINCE 01-Jan-2026"
        );
    }

    #[test]
    fn search_query_before() {
        assert_eq!(
            build_search_query(None, Some("01-Feb-2026")),
            "BEFORE 01-Feb-2026"
        );
    }

    /// Bounded windows chunk very large mailbox walks so a single IMAP
    /// session doesn't outlive the server's idle timeout.
    #[test]
    fn search_query_since_and_before() {
        assert_eq!(
            build_search_query(Some("01-Jan-2026"), Some("01-Feb-2026")),
            "SINCE 01-Jan-2026 BEFORE 01-Feb-2026"
        );
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

    fn fixture_extractors(names: &[&str]) -> Vec<crate::extractor::Extractor> {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extractors");
        crate::extractor::discover(&[dir])
            .unwrap()
            .into_iter()
            .filter(|ex| names.contains(&ex.name.as_str()))
            .collect()
    }

    #[test]
    fn from_restriction_for_a_single_extractor() {
        assert_eq!(
            build_from_restriction(&fixture_extractors(&["fixture-flight"])).as_deref(),
            Some(r#"HEADER FROM "flight.fixture.test""#)
        );
    }

    #[test]
    fn from_restriction_ors_several_domains() {
        // Two extractors, one domain each: a single binary OR.
        assert_eq!(
            build_from_restriction(&fixture_extractors(&["fixture-flight", "fixture-lodging"]))
                .as_deref(),
            Some(r#"OR HEADER FROM "flight.fixture.test" HEADER FROM "lodging.fixture.test""#)
        );
    }

    #[test]
    fn from_restriction_nests_ors_for_three_domains() {
        // N terms need N-1 prefix ORs; check the nesting shape.
        assert_eq!(
            build_from_restriction(&fixture_extractors(&[
                "fixture-flight",
                "fixture-lodging",
                "fixture-parcel-status",
            ]))
            .as_deref(),
            Some(
                r#"OR OR HEADER FROM "flight.fixture.test" HEADER FROM "lodging.fixture.test" HEADER FROM "parcel.fixture.test""#
            )
        );
    }

    #[test]
    fn from_restriction_is_none_when_an_extractor_names_no_senders() {
        // `fixture-ics-pass` declares only `requires:`, so it could
        // match a message from anyone; narrowing would hide those.
        assert_eq!(
            build_from_restriction(&fixture_extractors(&["fixture-flight", "fixture-ics-pass"])),
            None
        );
    }

    #[test]
    fn from_restriction_is_none_without_extractors() {
        assert_eq!(build_from_restriction(&[]), None);
    }

    #[test]
    fn quote_imap_string_escapes_quotes_and_backslashes() {
        assert_eq!(quote_imap_string("plain.example"), r#""plain.example""#);
        assert_eq!(quote_imap_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_imap_string(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn search_after_query_appends_the_from_restriction() {
        assert_eq!(
            search_after_query(42, Some(r#"HEADER FROM "x.example""#)),
            r#"UID 43:* HEADER FROM "x.example""#
        );
    }

    #[test]
    fn search_after_query_from_zero() {
        // Initial watch on an empty (or not-yet-scanned) mailbox.
        // `UID 1:*` is the standard "everything that exists" form.
        assert_eq!(search_after_query(0, None), "UID 1:*");
    }

    #[test]
    fn search_after_query_from_nonzero_cursor() {
        // Typical watch tick after some UIDs are already processed.
        assert_eq!(search_after_query(42, None), "UID 43:*");
    }

    #[test]
    fn search_after_query_saturates_at_u32_max() {
        // Defensive: u32::MAX as cursor would overflow naively. The
        // resulting query is silly (UID MAX:*) but the function must
        // not panic; the server will return an empty set.
        assert_eq!(
            search_after_query(u32::MAX, None),
            format!("UID {}:*", u32::MAX)
        );
    }

    #[test]
    fn xoauth2_client_response_format() {
        use imap::Authenticator;
        let auth = XOAuth2Authenticator {
            user: "someone@example.com",
            access_token: "ya29.a0AfH6SMB",
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
            user: "u@example.com",
            access_token: "tok",
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

    #[test]
    fn mailbox_not_found_survives_anyhow_downcast() {
        // The CLI recognises this via `err.is::<MailboxNotFound>()` to
        // print a plain message instead of a backtrace.
        let err: anyhow::Error = MailboxNotFound {
            mailbox: "archive".to_string(),
        }
        .into();
        assert!(err.is::<MailboxNotFound>());
        assert_eq!(err.to_string(), "no such mailbox: archive");
    }

    #[test]
    fn fetch_backoff_grows_to_half_an_hour() {
        // A throttled Gmail session stays throttled for hours, so the
        // fetch backoff has to climb well past the reconnect cap.
        let mut d = Duration::from_secs(1);
        let mut seen = vec![d];
        for _ in 0..12 {
            d = grow_backoff(d, FETCH_BACKOFF_MAX);
            seen.push(d);
        }
        assert_eq!(seen[0], Duration::from_secs(1));
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[10], Duration::from_secs(1024));
        // Capped, not unbounded.
        assert_eq!(d, FETCH_BACKOFF_MAX);
        assert_eq!(grow_backoff(d, FETCH_BACKOFF_MAX), FETCH_BACKOFF_MAX);
    }

    #[test]
    fn reconnect_backoff_still_caps_at_a_minute() {
        let mut d = Duration::from_secs(1);
        for _ in 0..10 {
            d = grow_backoff(d, RECONNECT_BACKOFF_MAX);
        }
        assert_eq!(d, RECONNECT_BACKOFF_MAX);
    }

    #[test]
    fn should_reload_holds_off_until_deadline() {
        let now = Instant::now();
        let past = now.checked_sub(Duration::from_millis(1)).unwrap_or(now);
        let future = now + Duration::from_secs(1);

        assert!(!should_reload(now, None), "no pending deadline");
        assert!(!should_reload(now, Some(future)), "deadline in the future");
        assert!(should_reload(now, Some(now)), "deadline is now");
        assert!(should_reload(now, Some(past)), "deadline in the past");
    }

    /// Build a temporary extractors directory with one manifest per
    /// name in `manifests` (mapped `name -> body`) and an empty
    /// executable script alongside each. Returned tempdir owns the
    /// tree so tests must keep it alive across `discover` calls.
    #[cfg(unix)]
    fn tmp_extractor_dir(manifests: &[(&str, &str)]) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        for (name, body) in manifests {
            std::fs::write(dir.path().join(format!("{name}.yaml")), body).unwrap();
            let script = dir.path().join(format!("{name}.py"));
            std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[cfg(unix)]
    #[test]
    fn diff_reloaded_reports_new_extractor() {
        let dir = tmp_extractor_dir(&[("a", "name: a\n"), ("b", "name: b\n")]);
        let first = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();
        // Seed the fingerprint map with just `a`; `b` should look new.
        let mut fingerprints = fingerprint_map(
            &first
                .iter()
                .filter(|e| e.name == "a")
                .cloned()
                .collect::<Vec<_>>(),
        );
        let changed = diff_reloaded(&mut fingerprints, &first);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].name, "b");
        // Map now covers both.
        assert!(fingerprints.contains_key("a"));
        assert!(fingerprints.contains_key("b"));
    }

    #[cfg(unix)]
    #[test]
    fn diff_reloaded_reports_manifest_edit() {
        let dir = tmp_extractor_dir(&[("a", "name: a\norder: 100\n")]);
        let before = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();
        let mut fingerprints = fingerprint_map(&before);

        std::fs::write(dir.path().join("a.yaml"), "name: a\norder: 42\n").unwrap();
        let after = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();

        let changed = diff_reloaded(&mut fingerprints, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].name, "a");
        assert_eq!(changed[0].order, 42);
    }

    #[cfg(unix)]
    #[test]
    fn diff_reloaded_ignores_unchanged() {
        let dir = tmp_extractor_dir(&[("a", "name: a\n")]);
        let loaded = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();
        let mut fingerprints = fingerprint_map(&loaded);

        let changed = diff_reloaded(&mut fingerprints, &loaded);
        assert!(changed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn diff_reloaded_drops_removed_extractor_from_state() {
        let dir = tmp_extractor_dir(&[("a", "name: a\n"), ("b", "name: b\n")]);
        let both = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();
        let mut fingerprints = fingerprint_map(&both);
        // Rediscover with `b` gone.
        std::fs::remove_file(dir.path().join("b.yaml")).unwrap();
        let just_a = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();

        let changed = diff_reloaded(&mut fingerprints, &just_a);
        assert!(changed.is_empty(), "removals don't retrigger anything");
        assert!(fingerprints.contains_key("a"));
        assert!(
            !fingerprints.contains_key("b"),
            "removed extractor drops from state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn diff_reloaded_picks_up_manifest_written_between_reloads() {
        // Exercise the exact "new extractor dropped into the dir
        // while --watch is running" flow: seed state from the first
        // discovery, drop a new manifest, rediscover, and expect
        // just the new one back.
        let dir = tmp_extractor_dir(&[("a", "name: a\n")]);
        let before = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();
        let mut fingerprints = fingerprint_map(&before);

        // Write a second manifest + script the way `tmp_extractor_dir`
        // does; the notify watcher would fire on this in production
        // but we short-circuit to the diff.
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.path().join("b.yaml"), "name: b\n").unwrap();
        let script = dir.path().join("b.py");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let after = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(after.len(), 2);

        let changed = diff_reloaded(&mut fingerprints, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].name, "b");
    }

    #[cfg(unix)]
    #[test]
    fn diff_reloaded_reports_only_the_changed_subset() {
        let dir = tmp_extractor_dir(&[
            ("a", "name: a\norder: 100\n"),
            ("b", "name: b\norder: 100\n"),
        ]);
        let before = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();
        let mut fingerprints = fingerprint_map(&before);
        // Touch only `b`.
        std::fs::write(dir.path().join("b.yaml"), "name: b\norder: 5\n").unwrap();
        let after = crate::extractor::discover(&[dir.path().to_path_buf()]).unwrap();

        let changed = diff_reloaded(&mut fingerprints, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].name, "b");
    }
}
