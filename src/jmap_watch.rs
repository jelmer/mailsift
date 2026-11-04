//! `mailsift jmap-watch`: run the pipeline over the mail in a JMAP
//! ([RFC 8620], [RFC 8621]) mailbox, and keep doing so as new mail
//! arrives.
//!
//! Every message we finish with gets a keyword (`$mailsift-seen` by
//! default), so the server itself remembers what is done and the query
//! for new work is just "in this mailbox, without that keyword". New mail
//! is noticed through the JMAP event source (RFC 8620 section 7.3).
//!
//! [RFC 8620]: https://www.rfc-editor.org/rfc/rfc8620
//! [RFC 8621]: https://www.rfc-editor.org/rfc/rfc8621

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use jmap_client::client::{Client, Credentials};
use jmap_client::email::{self, Property as EmailProperty};
use jmap_client::event_source::PushNotification;
use jmap_client::mailbox::{Property as MailboxProperty, Role};
use jmap_client::{DataType, core::query};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::imap_scan::MailboxNotFound;
use crate::pipeline::{self, PipelineTargets};

pub const DEFAULT_SEEN_KEYWORD: &str = "$mailsift-seen";

/// Even without a notification, drop the event source and query again
/// this often. The event source swallows the server's pings, so a
/// connection that died silently would otherwise go unnoticed.
const RESYNC_INTERVAL: Duration = Duration::from_secs(300);

/// Ask the server to send a ping this often (seconds) to keep
/// intermediaries from closing the event source connection.
const PING_INTERVAL: u32 = 60;

const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);

pub struct JmapWatchConfig<'a> {
    /// `scheme://host[:port]`; the session is fetched from
    /// `/.well-known/jmap` under it.
    pub base_url: &'a str,
    /// Host the session redirect may point at.
    pub host: &'a str,
    /// Slash-separated mailbox path (`Receipts/2026`), or `None` for the
    /// inbox.
    pub mailbox: Option<&'a str>,
    pub seen_keyword: &'a str,
    pub extractors: &'a [crate::extractor::Extractor],
    pub targets: PipelineTargets<'a>,
    /// Run the pipeline in dry-run mode and leave the keyword unset.
    pub dry_run: bool,
    /// Keep running and process new mail as it arrives. Without it,
    /// process what is there and exit.
    pub watch: bool,
    pub runtime: &'a Handle,
}

pub fn run(config: JmapWatchConfig<'_>, credentials: Credentials) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let wakeup = Arc::new(Notify::new());
    if config.watch {
        let flag = Arc::clone(&interrupted);
        let wakeup = Arc::clone(&wakeup);
        if let Err(e) = ctrlc::set_handler(move || {
            flag.store(true, Ordering::SeqCst);
            wakeup.notify_one();
        }) {
            warn!(error = %e, "could not install SIGINT handler; Ctrl-C may not exit cleanly");
        }
    }

    let rt = config.runtime;
    let client = rt
        .block_on(
            Client::new()
                .credentials(credentials)
                .follow_redirects([config.host])
                .connect(config.base_url),
        )
        .with_context(|| format!("connecting to JMAP server {}", config.base_url))?;

    let mailbox_id = find_mailbox(&client, rt, config.mailbox)?;
    info!(
        mailbox = config.mailbox.unwrap_or("inbox"),
        id = mailbox_id,
        "watching"
    );

    // Messages we've given up on (or, in dry-run mode, already looked at)
    // this run. They keep their place in the query results, so they're
    // skipped over rather than refetched.
    let mut skip = HashSet::new();

    if !config.watch {
        return process_pending(&client, &config, &mailbox_id, &mut skip);
    }

    let mut backoff = Duration::from_secs(1);
    while !interrupted.load(Ordering::SeqCst) {
        match watch_once(&client, &config, &mailbox_id, &mut skip, &wakeup) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                warn!(
                    error = format!("{e:#}"),
                    ?backoff,
                    "watching failed; reconnecting"
                );
                wait(rt, &wakeup, backoff);
                backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
            }
        }
    }
    info!("interrupted; exiting");
    Ok(())
}

/// Subscribe to the event source and process mail as it is reported, until
/// Ctrl-C, the next resync or an error.
fn watch_once(
    client: &Client,
    config: &JmapWatchConfig<'_>,
    mailbox_id: &str,
    skip: &mut HashSet<String>,
    wakeup: &Notify,
) -> Result<()> {
    let rt = config.runtime;
    // Subscribe before querying, so mail that arrives in between still
    // triggers a notification.
    let mut stream = rt
        .block_on(client.event_source(Some([DataType::Email]), false, Some(PING_INTERVAL), None))
        .context("opening the JMAP event source")?;
    process_pending(client, config, mailbox_id, skip)?;
    loop {
        let next = rt.block_on(async {
            tokio::select! {
                item = stream.next() => Some(item),
                _ = wakeup.notified() => None,
                _ = tokio::time::sleep(RESYNC_INTERVAL) => None,
            }
        });
        match next {
            None => return Ok(()),
            Some(None) => bail!("the JMAP event source closed"),
            Some(Some(Ok(PushNotification::StateChange(changes))))
                if changes.has_type(DataType::Email) =>
            {
                debug!("email state changed");
                process_pending(client, config, mailbox_id, skip)?;
            }
            Some(Some(Ok(_))) => {}
            Some(Some(Err(e))) => return Err(e).context("reading the JMAP event source"),
        }
    }
}

/// Sleep for `duration`, or until Ctrl-C.
fn wait(rt: &Handle, wakeup: &Notify, duration: Duration) {
    rt.block_on(async {
        tokio::select! {
            _ = wakeup.notified() => {}
            _ = tokio::time::sleep(duration) => {}
        }
    });
}

/// Run the pipeline over every message in the mailbox that doesn't carry
/// the seen keyword yet, oldest first.
fn process_pending(
    client: &Client,
    config: &JmapWatchConfig<'_>,
    mailbox_id: &str,
    skip: &mut HashSet<String>,
) -> Result<()> {
    let rt = config.runtime;
    let page_size = client
        .session()
        .core_capabilities()
        .context("JMAP session has no core capability")?
        .max_objects_in_get();
    // Messages before this position are still unmarked (failed or
    // dry-run) and would otherwise be returned again.
    let mut position: i32 = 0;
    loop {
        let mut request = client.build();
        request
            .query_email()
            .filter(query::Filter::and([
                email::query::Filter::in_mailbox(mailbox_id),
                email::query::Filter::not_keyword(config.seen_keyword),
            ]))
            .sort([email::query::Comparator::received_at().ascending()])
            .position(position)
            .limit(page_size);
        let ids = rt
            .block_on(request.send_query_email())
            .context("Email/query")?
            .take_ids();
        if ids.is_empty() {
            return Ok(());
        }

        let mut request = client.build();
        request
            .get_email()
            .ids(ids.iter().map(String::as_str))
            .properties([
                EmailProperty::Id,
                EmailProperty::BlobId,
                EmailProperty::ReceivedAt,
            ]);
        let emails = rt
            .block_on(request.send_get_email())
            .context("Email/get")?
            .take_list();

        for message in emails {
            let id = message
                .id()
                .context("server returned an email without an id")?;
            if skip.contains(id) {
                position += 1;
                continue;
            }
            let blob_id = message
                .blob_id()
                .with_context(|| format!("email {id} has no blobId"))?;
            let raw = rt
                .block_on(client.download(blob_id))
                .with_context(|| format!("downloading email {id}"))?;
            debug!(id, size = raw.len(), "processing");
            let result = pipeline::run(
                &raw,
                &format!("JMAP {id}"),
                config.extractors,
                config.targets,
                // receivedAt is set by the server, not the sender.
                pipeline::DkimPolicy::Enforce {
                    received_at: message.received_at(),
                },
                config.dry_run,
                None,
            );
            if result.is_ok() && !config.dry_run {
                rt.block_on(client.email_set_keyword(id, config.seen_keyword, true))
                    .with_context(|| format!("setting {} on email {id}", config.seen_keyword))?;
                continue;
            }
            if let Err(e) = result {
                // Left unmarked, so a restart tries again.
                warn!(
                    id,
                    error = format!("{e:#}"),
                    "pipeline failed; leaving the message unmarked"
                );
            }
            skip.insert(id.to_string());
            position += 1;
        }
    }
}

fn find_mailbox(client: &Client, rt: &Handle, path: Option<&str>) -> Result<String> {
    let mut request = client.build();
    request.get_mailbox().properties([
        MailboxProperty::Id,
        MailboxProperty::Name,
        MailboxProperty::ParentId,
        MailboxProperty::Role,
    ]);
    let mailboxes = rt
        .block_on(request.send_get_mailbox())
        .context("Mailbox/get")?
        .take_list();
    let entries = mailboxes
        .iter()
        .map(|m| {
            Ok(MailboxEntry {
                id: m.id().context("server returned a mailbox without an id")?,
                name: m.name().unwrap_or(""),
                parent_id: m.parent_id(),
                is_inbox: matches!(m.role(), Role::Inbox),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    resolve_mailbox(&entries, path).map(str::to_string)
}

struct MailboxEntry<'a> {
    id: &'a str,
    name: &'a str,
    parent_id: Option<&'a str>,
    is_inbox: bool,
}

/// Find the id of the mailbox at `path` (names separated by `/`, from the
/// top level down), or of the inbox when `path` is `None`.
///
/// Matching is done here rather than with a `Mailbox/query` name filter,
/// since RFC 8621 defines that filter as a substring match.
fn resolve_mailbox<'a>(mailboxes: &[MailboxEntry<'a>], path: Option<&str>) -> Result<&'a str> {
    let not_found = |name: &str| MailboxNotFound {
        mailbox: name.to_string(),
    };
    let Some(path) = path else {
        return mailboxes
            .iter()
            .find(|m| m.is_inbox)
            .map(|m| m.id)
            .ok_or_else(|| not_found("inbox").into());
    };
    let mut parent: Option<&str> = None;
    for component in path.split('/') {
        let mut matches = mailboxes
            .iter()
            .filter(|m| m.parent_id == parent && m.name == component);
        let found = matches.next().ok_or_else(|| not_found(path))?;
        if matches.next().is_some() {
            bail!("mailbox path {path:?} is ambiguous: several mailboxes are named {component:?}");
        }
        parent = Some(found.id);
    }
    parent.ok_or_else(|| not_found(path).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Vec<MailboxEntry<'static>> {
        vec![
            MailboxEntry {
                id: "a",
                name: "Inbox",
                parent_id: None,
                is_inbox: true,
            },
            MailboxEntry {
                id: "b",
                name: "Receipts",
                parent_id: None,
                is_inbox: false,
            },
            MailboxEntry {
                id: "c",
                name: "2026",
                parent_id: Some("b"),
                is_inbox: false,
            },
            MailboxEntry {
                id: "d",
                name: "2026",
                parent_id: None,
                is_inbox: false,
            },
        ]
    }

    #[test]
    fn resolves_the_inbox_by_role() {
        assert_eq!(resolve_mailbox(&tree(), None).unwrap(), "a");
    }

    #[test]
    fn resolves_a_top_level_mailbox() {
        assert_eq!(resolve_mailbox(&tree(), Some("2026")).unwrap(), "d");
    }

    #[test]
    fn resolves_a_nested_mailbox() {
        assert_eq!(
            resolve_mailbox(&tree(), Some("Receipts/2026")).unwrap(),
            "c"
        );
    }

    #[test]
    fn a_missing_component_is_not_found() {
        let err = resolve_mailbox(&tree(), Some("Receipts/2025")).unwrap_err();
        assert_eq!(err.to_string(), "no such mailbox: Receipts/2025");
        assert!(err.is::<MailboxNotFound>());
    }

    #[test]
    fn duplicate_names_are_ambiguous() {
        let mut mailboxes = tree();
        mailboxes.push(MailboxEntry {
            id: "e",
            name: "Receipts",
            parent_id: None,
            is_inbox: false,
        });
        let err = resolve_mailbox(&mailboxes, Some("Receipts")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "mailbox path \"Receipts\" is ambiguous: several mailboxes are named \"Receipts\""
        );
    }
}
