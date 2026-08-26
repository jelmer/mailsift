use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};

use mailsift::cli::{
    CaldavTarget, current_username, default_config_path, parse_caldav_url, parse_imap_url,
};
use mailsift::config::{CaldavConfig, Config};
use mailsift::pipeline::DkimPolicy;
use mailsift::targets::{EventSinkKind, caldav};
use mailsift::{imap_scan, milter, pipeline};

const DEFAULT_EXTRACTORS_DIR: &str = "extractors";

/// A mistake in how the user invoked us (missing/conflicting flags, an
/// unparseable URL) rather than an unexpected failure. Reported as a plain
/// one-line message with no backtrace; genuine errors keep anyhow's default
/// `Debug` output.
#[derive(Debug)]
struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

macro_rules! usage_error {
    ($($arg:tt)*) => {
        anyhow::Error::new(UsageError(format!($($arg)*)))
    };
}

/// Read a password / API-token file, trim, and return its contents.
/// Used by every sink that reads a secret from disk (CalDAV/WebDAV
/// passwords, SMTP password, Firefly / Karrio / 17track tokens).
fn read_secret_file(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_string())
}

/// Resolve the username for XOAUTH2 auth, which the SASL message needs.
/// XOAUTH2 has no equivalent of a username-less GSSAPI ticket, so unlike
/// `LOGIN` we can't fall back to nothing: the account must be in the URL.
fn xoauth2_user(resolved_user: Option<&str>) -> Result<&str> {
    resolved_user.ok_or_else(|| {
        usage_error!(
            "XOAUTH2 requires a username; include one in the URL \
             (imaps://you@imap.gmail.com/INBOX)"
        )
    })
}

/// clap value parser for `--socket-mode`. Accepts octal digits with or
/// without a `0`/`0o` prefix (`0666`, `0o660`, `660` all mean 0o660).
#[cfg(feature = "web")]
fn parse_octal_mode(raw: &str) -> Result<u32, String> {
    let s = raw.trim_start_matches("0o").trim_start_matches('0');
    let s = if s.is_empty() { "0" } else { s };
    u32::from_str_radix(s, 8).map_err(|e| format!("invalid octal mode {raw:?}: {e}"))
}

/// The `--oauth2-*` flags of `imap-scan`, borrowed for
/// [`build_token_provider`].
struct OAuth2ScanFlags<'a> {
    credentials_file: Option<&'a Path>,
    refresh_token_file: Option<&'a Path>,
    client_id: Option<&'a str>,
    client_secret_file: Option<&'a Path>,
    provider_name: Option<&'a str>,
    token_endpoint: Option<&'a str>,
}

/// Build an OAuth2 [`TokenProvider`](mailsift::oauth2::TokenProvider)
/// from the `--oauth2-*` flags, or return `None` if neither a credential
/// bundle nor a refresh token was given (the caller then falls back to
/// the other auth methods).
///
/// A `--oauth2-credentials-file` is self-contained. Otherwise the token
/// endpoint is taken from `--oauth2-token-endpoint` if given, else from
/// `--oauth2-provider`, else derived from the IMAP host. An unrecognised
/// host with no explicit endpoint is a usage error rather than a silent
/// guess.
fn build_token_provider(
    flags: &OAuth2ScanFlags<'_>,
    imap_host: &str,
    runtime: &tokio::runtime::Handle,
) -> Result<Option<mailsift::oauth2::TokenProvider>> {
    use mailsift::oauth2::{OAuth2Config, Provider, TokenProvider};

    // A credential bundle is self-contained; use it directly.
    if let Some(path) = flags.credentials_file {
        let config = OAuth2Config::load(path)?;
        return Ok(Some(TokenProvider::new(config, runtime.clone())));
    }

    let Some(refresh_token_file) = flags.refresh_token_file else {
        return Ok(None);
    };
    // clap's `requires = "oauth2_client_id"` guarantees this, but the
    // handler shouldn't assume the parser's invariants hold.
    let client_id = flags
        .client_id
        .ok_or_else(|| usage_error!("--oauth2-refresh-token-file requires --oauth2-client-id"))?;

    let endpoint = match (flags.token_endpoint, flags.provider_name) {
        (Some(explicit), _) => explicit.to_string(),
        (None, Some(name)) => Provider::from_name(name)
            .ok_or_else(|| {
                usage_error!(
                    "unknown --oauth2-provider {name:?}; use google or microsoft, \
                     or pass --oauth2-token-endpoint"
                )
            })?
            .token_endpoint()
            .to_string(),
        (None, None) => Provider::from_imap_host(imap_host)
            .ok_or_else(|| {
                usage_error!(
                    "cannot derive an OAuth2 provider from IMAP host {imap_host:?}; \
                     pass --oauth2-provider or --oauth2-token-endpoint"
                )
            })?
            .token_endpoint()
            .to_string(),
    };

    let refresh_token = read_secret_file(refresh_token_file)?;
    let client_secret = flags.client_secret_file.map(read_secret_file).transpose()?;
    Ok(Some(TokenProvider::new(
        OAuth2Config {
            token_endpoint: endpoint,
            client_id: client_id.to_string(),
            client_secret,
            refresh_token,
        },
        runtime.clone(),
    )))
}

/// Handle `mailsift imap-auth`: run the browser consent flow and write
/// the credential bundle.
fn run_imap_auth(args: ImapAuthArgs, runtime: &tokio::runtime::Handle) -> Result<()> {
    use mailsift::oauth2::{ConsentRequest, Provider, consent};

    let provider = match args.provider.as_deref() {
        Some(name) => Provider::from_name(name)
            .ok_or_else(|| usage_error!("unknown --provider {name:?}; use google or microsoft"))?,
        None => {
            let domain = args.account.rsplit('@').next().filter(|d| !d.is_empty());
            // Only Gmail/Outlook domains are auto-derivable; anything
            // else must name the provider explicitly.
            match domain {
                Some(d) if d.eq_ignore_ascii_case("gmail.com") => Provider::Google,
                Some(d)
                    if d.eq_ignore_ascii_case("outlook.com")
                        || d.eq_ignore_ascii_case("hotmail.com")
                        || d.eq_ignore_ascii_case("live.com") =>
                {
                    Provider::Microsoft
                }
                _ => {
                    return Err(usage_error!(
                        "cannot derive the provider from {:?}; pass --provider google|microsoft",
                        args.account
                    ));
                }
            }
        }
    };

    let client_secret = args
        .client_secret_file
        .as_deref()
        .map(read_secret_file)
        .transpose()?;

    let config = consent(
        ConsentRequest {
            auth_endpoint: provider.auth_endpoint(),
            token_endpoint: provider.token_endpoint(),
            client_id: &args.client_id,
            client_secret: client_secret.as_deref(),
            scope: provider.consent_scope(),
            login_hint: Some(&args.account),
            no_browser: args.no_browser,
        },
        runtime,
    )?;

    config.save(&args.output)?;
    println!(
        "Wrote OAuth2 credentials to {}. Use it with:\n  \
         mailsift imap-scan imaps://{}@<host>/INBOX --oauth2-credentials-file {}",
        args.output.display(),
        args.account,
        args.output.display()
    );
    Ok(())
}

#[derive(Parser)]
#[command(name = "mailsift", version, about)]
struct Cli {
    /// TOML config file with defaults for the CLI flags. Defaults to
    /// `$XDG_CONFIG_HOME/mailsift/config.toml` when present.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// Where to send `event` artifacts. Either a local directory or a CalDAV
/// collection; exactly one must be resolvable after CLI + config merge.
#[derive(Args, Clone, Default)]
struct EventTargetArgs {
    /// File events as `<UID>.ics` under this directory.
    #[arg(long)]
    events_dir: Option<PathBuf>,
    /// PUT events to this CalDAV server. The sink runs PROPFIND from
    /// this URL to find the user's schedule inbox and default
    /// calendar; the server root is usually enough. May include a
    /// username (`https://user@host/`); the password (if any) comes
    /// from `--caldav-password-file` or the config file.
    #[arg(long)]
    caldav_url: Option<String>,
    /// File containing the CalDAV password. With the `gssapi` feature,
    /// omit this to use Kerberos from the caller's credential cache.
    #[arg(long)]
    caldav_password_file: Option<PathBuf>,
}

impl EventTargetArgs {
    fn build_sink(
        &self,
        config: &Config,
        runtime: &tokio::runtime::Handle,
    ) -> Result<EventSinkKind> {
        // A CLI flag for one target picks that target exclusively and
        // ignores config defaults for the other. Without this, a config
        // file that sets `[caldav]` would clash with someone passing
        // `--events-dir` on the command line.
        let (events_dir, caldav_url) = match (&self.events_dir, &self.caldav_url) {
            (Some(_), Some(_)) => {
                return Err(usage_error!(
                    "specify either --events-dir or a CalDAV target, not both"
                ));
            }
            (Some(dir), None) => (Some(dir.clone()), None),
            (None, Some(url)) => (None, Some(url.clone())),
            (None, None) => (
                config.events_dir.clone(),
                config.caldav.as_ref().map(|c: &CaldavConfig| c.url.clone()),
            ),
        };

        match (events_dir, caldav_url) {
            (Some(_), Some(_)) => Err(usage_error!(
                "config specifies both events_dir and [caldav]; pick one"
            )),
            (Some(dir), None) => Ok(EventSinkKind::LocalDir(dir)),
            (None, Some(raw_url)) => {
                let CaldavTarget { url, user } = parse_caldav_url(&raw_url)?;
                let user = user.or_else(|| config.caldav.as_ref().and_then(|c| c.user.clone()));
                let password_file = self
                    .caldav_password_file
                    .clone()
                    .or_else(|| config.caldav.as_ref().and_then(|c| c.password_file.clone()));
                let password = password_file.as_deref().map(read_secret_file).transpose()?;
                Ok(EventSinkKind::Caldav(caldav::CaldavSink::new(
                    url,
                    user,
                    password,
                    runtime.clone(),
                )?))
            }
            (None, None) => Err(usage_error!(
                "no event target specified: pass --events-dir, --caldav-url, or set one in the config"
            )),
        }
    }
}

/// Arguments for the `imap-scan` subcommand. Split out of the
/// [`Command`] enum (and boxed there) because its flag set is far larger
/// than any other variant's.
#[derive(Args)]
struct ImapScanArgs {
    /// IMAP URL: `imaps://[user@]host[:port]/[mailbox]`. Without a
    /// user the current OS user is used; without a mailbox `INBOX`
    /// is used.
    url: String,
    /// File containing the IMAP password. Provide for plain LOGIN;
    /// omit to authenticate via Kerberos (`gssapi` feature only) or
    /// pass `--oauth2-token-file` for SASL XOAUTH2 (Gmail etc.).
    #[arg(long, conflicts_with_all = ["oauth2_token_file", "oauth2_refresh_token_file"], help_heading = "Authentication")]
    password_file: Option<PathBuf>,
    /// File containing a fixed OAuth2 bearer token for SASL XOAUTH2.
    /// The URL must include the account
    /// (`imaps://you@imap.gmail.com/INBOX`). Tokens expire, so this
    /// is only suitable for a one-off scan that finishes within the
    /// token's lifetime; for `--watch` use
    /// `--oauth2-refresh-token-file` instead. Mutually exclusive with
    /// the refresh-token flags.
    #[arg(
        long,
        conflicts_with = "oauth2_refresh_token_file",
        help_heading = "Authentication"
    )]
    oauth2_token_file: Option<PathBuf>,
    /// JSON credential bundle from `mailsift imap-auth` (client id,
    /// secret, refresh token, and token endpoint in one file). The
    /// self-contained equivalent of the `--oauth2-refresh-token-file` +
    /// `--oauth2-client-*` flags; mints a fresh access token at every
    /// (re)connect, so it suits `--watch`. The URL must include the
    /// account.
    #[arg(
        long,
        conflicts_with_all = ["password_file", "oauth2_token_file", "oauth2_refresh_token_file"],
        help_heading = "Authentication"
    )]
    oauth2_credentials_file: Option<PathBuf>,
    /// File containing a long-lived OAuth2 refresh token for SASL
    /// XOAUTH2. mailsift mints a fresh access token from it at every
    /// (re)connect, so this survives token expiry across a `--watch`
    /// session. Requires `--oauth2-client-id`; the provider (token
    /// endpoint) is derived from the IMAP host for Gmail/Outlook and
    /// can be overridden with `--oauth2-provider` /
    /// `--oauth2-token-endpoint`. The URL must include the account.
    #[arg(long, requires = "oauth2_client_id", help_heading = "Authentication")]
    oauth2_refresh_token_file: Option<PathBuf>,
    /// OAuth2 client id of the registered application. Required with
    /// `--oauth2-refresh-token-file`.
    #[arg(long, help_heading = "Authentication")]
    oauth2_client_id: Option<String>,
    /// File containing the OAuth2 client secret. Google desktop apps
    /// need one; Microsoft public (native) clients must omit it.
    #[arg(long, help_heading = "Authentication")]
    oauth2_client_secret_file: Option<PathBuf>,
    /// Override the OAuth2 provider instead of deriving it from the
    /// IMAP host. One of `google` or `microsoft`. Rarely needed; use
    /// it when the host isn't recognised but is one of these
    /// providers, or use `--oauth2-token-endpoint` for anything else.
    #[arg(long, help_heading = "Authentication")]
    oauth2_provider: Option<String>,
    /// OAuth2 token endpoint URL, for a provider not derivable from
    /// the IMAP host (anything other than Gmail/Outlook). Takes
    /// precedence over `--oauth2-provider` and host derivation.
    #[arg(long, help_heading = "Authentication")]
    oauth2_token_endpoint: Option<String>,
    /// Optional SASL authorization identity for GSSAPI auth.
    #[arg(long, help_heading = "Authentication")]
    authzid: Option<String>,
    /// Only consider messages from this date onward, in IMAP date
    /// format (e.g. `01-Jan-2026`).
    #[arg(long)]
    since: Option<String>,
    /// Cap on the number of messages to process in the initial
    /// scan. With `--watch`, the cap applies only to the backfill
    /// pass; messages arriving while watching are not counted.
    #[arg(long)]
    limit: Option<usize>,
    /// Directory containing extractor scripts.
    #[arg(long)]
    extractors: Option<PathBuf>,
    #[command(flatten)]
    target: EventTargetArgs,
    #[command(flatten)]
    artifacts: ArtifactDirArgs,
    #[command(flatten)]
    trackers: TrackerArgs,
    #[command(flatten)]
    firefly: FireflyArgs,
    /// Don't actually file artifacts; just report what would happen.
    #[arg(long)]
    dry_run: bool,
    /// After the initial scan, stay connected and process new
    /// messages as they arrive (IMAP IDLE, RFC 2177). Runs until
    /// interrupted (Ctrl-C). On transport errors the connection is
    /// rebuilt with exponential backoff.
    #[arg(long)]
    watch: bool,
}

/// Arguments for the `imap-auth` subcommand.
#[derive(Args)]
struct ImapAuthArgs {
    /// The email account to authorize (`you@gmail.com`). Used to derive
    /// the provider when `--provider` is omitted and to pre-fill the
    /// consent screen.
    account: String,
    /// OAuth2 provider: `google` or `microsoft`. Derived from the
    /// account's domain when omitted.
    #[arg(long)]
    provider: Option<String>,
    /// OAuth2 client id of your registered application.
    #[arg(long)]
    client_id: String,
    /// File containing the OAuth2 client secret. Google desktop apps
    /// need one; Microsoft public (native) clients omit it.
    #[arg(long)]
    client_secret_file: Option<PathBuf>,
    /// Where to write the JSON credential bundle. Pass this file to
    /// `imap-scan --oauth2-credentials-file`. Created with owner-only
    /// permissions.
    #[arg(long, short)]
    output: PathBuf,
    /// Don't start a local server or open a browser; print the URL and
    /// read the redirected URL back from stdin. For headless / SSH use.
    #[arg(long)]
    no_browser: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the pipeline against a saved .eml file.
    Replay {
        /// Path to the .eml file. Use "-" for stdin.
        path: PathBuf,
        /// Directory containing extractor scripts.
        #[arg(long)]
        extractors: Option<PathBuf>,
        #[command(flatten)]
        target: EventTargetArgs,
        #[command(flatten)]
        artifacts: ArtifactDirArgs,
        #[command(flatten)]
        trackers: TrackerArgs,
        #[command(flatten)]
        firefly: FireflyArgs,
        /// Don't actually file artifacts; just report what would happen.
        #[arg(long)]
        dry_run: bool,
        /// Print a per-extractor dispatch table after the run: which
        /// extractors matched, which were prefiltered out and why, and
        /// what artifacts each producing extractor emitted.
        #[arg(long)]
        explain: bool,
    },
    /// Walk an IMAP mailbox and run the pipeline over each message.
    // Boxed: this variant's args dwarf the others, so an unboxed
    // struct-variant trips clippy's `large_enum_variant`. See
    // [`ImapScanArgs`].
    ImapScan(Box<ImapScanArgs>),
    /// Obtain an OAuth2 refresh token via the browser consent flow and
    /// write a credential bundle for `imap-scan --oauth2-credentials-file`.
    ImapAuth(ImapAuthArgs),
    /// Walk a Maildir on disk and run the pipeline over each message.
    ///
    /// Useful for one-off backfills against archived mail. Reads
    /// `cur/` and `new/` (`tmp/` is skipped) and, with `--recurse`,
    /// descends into Maildir++ subfolders (`.name/cur`, `.name/new`).
    MaildirScan {
        /// Path to the Maildir root (contains `cur/`, `new/`, `tmp/`).
        path: PathBuf,
        /// Also process Maildir++ subfolders (`.foo/cur`, `.foo/new`).
        #[arg(long)]
        recurse: bool,
        /// Skip messages whose file mtime is older than this
        /// `YYYY-MM-DD` date.
        #[arg(long)]
        since: Option<String>,
        /// Cap on the number of messages to process.
        #[arg(long)]
        limit: Option<usize>,
        /// Directory containing extractor scripts.
        #[arg(long)]
        extractors: Option<PathBuf>,
        #[command(flatten)]
        target: EventTargetArgs,
        #[command(flatten)]
        artifacts: ArtifactDirArgs,
        #[command(flatten)]
        trackers: TrackerArgs,
        #[command(flatten)]
        firefly: FireflyArgs,
        /// Don't actually file artifacts; just report what would happen.
        #[arg(long)]
        dry_run: bool,
    },
    /// Validate every discoverable extractor manifest. Parses each
    /// `<name>.yaml`, compiles the `subject_regex`, parses `requires:`
    /// entries, and checks the named script is executable. Reports the
    /// final dispatch order and exits non-zero if anything is wrong.
    Check {
        /// Directory containing extractor scripts.
        #[arg(long)]
        extractors: Option<PathBuf>,
    },
    /// Validate every extractor manifest and report every issue at
    /// once (where `check` bails on the first failure). Useful as a
    /// pre-commit check: regex compilation, `requires:` shape,
    /// script presence + executability, and cross-directory name
    /// collisions are all reported with file paths.
    Lint {
        /// Directory containing extractor scripts.
        #[arg(long)]
        extractors: Option<PathBuf>,
    },
    /// Aggregate the milter's event log into a per-extractor summary
    /// (total runs, produced/empty/failed counts, mean wall-clock
    /// time, most-recent sender domains). Reads the NDJSON log the
    /// milter writes to `$XDG_STATE_HOME/mailsift/events.ndjson`.
    Stats {
        /// Path to the event log. Defaults to the same XDG state path
        /// the milter writes to.
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Serve a read-only HTTP dashboard over the configured artifact
    /// directories (events, bills, parcels, receipts, subscriptions,
    /// tickets). Rescans on every request; safe to leave running
    /// alongside the milter or an `imap-scan --watch`.
    #[cfg(feature = "web")]
    Web {
        /// Listen address: `host:port` for TCP or `unix:/path/to/sock`
        /// for a unix socket.
        #[arg(long, default_value = "127.0.0.1:8088")]
        listen: String,
        /// Permission bits for the unix socket, in octal. Default 0666
        /// so a reverse proxy running as a different user can connect;
        /// tighten (e.g. 0660 with a shared group) for multi-user hosts.
        /// Ignored for TCP listens.
        #[arg(long, value_parser = parse_octal_mode)]
        socket_mode: Option<u32>,
        /// URL prefix under which the app is mounted (e.g. `/mailsift`).
        /// Every generated link is prefixed with this so the app works
        /// behind a reverse proxy that only forwards a sub-path. Empty
        /// or `/` means serve at the site root.
        #[arg(long, default_value = "")]
        base_path: String,
    },
    /// Run as a Postfix milter.
    Milter {
        /// Listen address: `unix:/path/to/sock` or `tcp:host:port`.
        #[arg(long)]
        socket: String,
        /// Directory containing extractor scripts.
        #[arg(long)]
        extractors: Option<PathBuf>,
        #[command(flatten)]
        target: EventTargetArgs,
        #[command(flatten)]
        artifacts: ArtifactDirArgs,
        #[command(flatten)]
        trackers: TrackerArgs,
        #[command(flatten)]
        firefly: FireflyArgs,
        /// Hard wall-clock budget per message, in seconds. On exceed we
        /// log a warning and accept the mail anyway.
        #[arg(long, default_value_t = 20)]
        deadline_secs: u64,
    },
}

/// Per-subcommand Firefly-III flags. When both are set (CLI or
/// config), every filed bill is also registered with Firefly III.
#[derive(Args, Clone, Default)]
struct FireflyArgs {
    /// Base URL of a Firefly III instance. When set, newly-filed bills
    /// are also registered with it.
    #[arg(long)]
    firefly_url: Option<String>,
    /// File containing a Firefly Personal Access Token. Required when
    /// `--firefly-url` is set.
    #[arg(long)]
    firefly_token_file: Option<PathBuf>,
}

/// Per-subcommand tracker-registration flags. Each new
/// `--karrio-*` / `--seventeentrack-*` etc. group lives here.
#[derive(Args, Clone, Default)]
struct TrackerArgs {
    /// Base URL of a Karrio instance. When set, newly-seen parcel
    /// tracking numbers are registered with it.
    #[arg(long)]
    karrio_url: Option<String>,
    /// File containing the Karrio API token (a Karrio personal access
    /// token). Required when `--karrio-url` is set.
    #[arg(long)]
    karrio_token_file: Option<PathBuf>,
    /// File containing a 17track API token. When set, newly-seen
    /// parcel tracking numbers are also registered with 17track.
    #[arg(long)]
    seventeentrack_token_file: Option<PathBuf>,
}

/// Per-subcommand directories for non-event artifact kinds. Flatten this
/// onto every subcommand so the per-kind flags live in one place.
#[derive(Args, Clone, Default)]
struct ArtifactDirArgs {
    /// Directory under which to file `bill` artifacts. If omitted,
    /// bill artifacts are dropped with a warning.
    #[arg(long)]
    bills_dir: Option<PathBuf>,
    /// Directory under which to file `parcel` artifacts. If omitted,
    /// parcel artifacts are dropped with a warning.
    #[arg(long)]
    parcels_dir: Option<PathBuf>,
    /// Directory under which to file `subscription` artifacts. If
    /// omitted, subscription artifacts are dropped with a warning.
    #[arg(long)]
    subscriptions_dir: Option<PathBuf>,
    /// Directory under which to file `receipt` artifacts. Mutually
    /// exclusive with `--receipts-webdav-url` and
    /// `--receipts-forward-to`. If none is set, receipt artifacts are
    /// dropped with a warning.
    #[arg(long)]
    receipts_dir: Option<PathBuf>,
    /// WebDAV collection URL to PUT `receipt` artifacts to. May embed a
    /// username (`https://user@host/path/`). Mutually exclusive with
    /// `--receipts-dir` and `--receipts-forward-to`.
    #[arg(long)]
    receipts_webdav_url: Option<String>,
    /// File containing the password for the receipts WebDAV target.
    /// Optional with the `gssapi` feature (Kerberos from the ticket
    /// cache).
    #[arg(long)]
    receipts_webdav_password_file: Option<PathBuf>,
    /// Mailbox to forward receipt-emitting messages to. Each `receipt`
    /// artifact triggers a forwarded copy of the original RFC822
    /// message. Mutually exclusive with `--receipts-dir` and
    /// `--receipts-webdav-url`. Requires `--receipts-forward-from` plus
    /// one of `--receipts-forward-sendmail` / `--receipts-forward-smtp-url`.
    #[arg(long, value_delimiter = ',')]
    receipts_forward_to: Vec<String>,
    /// `From:` mailbox to put on forwarded receipt mails.
    #[arg(long)]
    receipts_forward_from: Option<String>,
    /// Path to a sendmail binary (e.g. `/usr/sbin/sendmail`). Mutually
    /// exclusive with `--receipts-forward-smtp-url`.
    #[arg(long)]
    receipts_forward_sendmail: Option<PathBuf>,
    /// SMTP submission URL (`smtps://[user@]host[:port]`, `smtp://`,
    /// `submissions://`). Mutually exclusive with
    /// `--receipts-forward-sendmail`. Requires the `smtp` Cargo
    /// feature.
    #[arg(long)]
    receipts_forward_smtp_url: Option<String>,
    /// Password file for the SMTP submission auth.
    #[arg(long)]
    receipts_forward_smtp_password_file: Option<PathBuf>,
    /// Directory under which to file `ticket` artifacts. Mutually
    /// exclusive with `--tickets-webdav-url`. If neither is set, ticket
    /// artifacts are dropped with a warning.
    #[arg(long)]
    tickets_dir: Option<PathBuf>,
    /// WebDAV collection URL to PUT `ticket` artifacts to. May embed a
    /// username (`https://user@host/path/`). Mutually exclusive with
    /// `--tickets-dir`.
    #[arg(long)]
    tickets_webdav_url: Option<String>,
    /// File containing the password for the tickets WebDAV target.
    /// Optional with the `gssapi` feature (Kerberos from the ticket
    /// cache).
    #[arg(long)]
    tickets_webdav_password_file: Option<PathBuf>,
}

impl ArtifactDirArgs {
    fn resolve(
        &self,
        config: &Config,
        runtime: &tokio::runtime::Handle,
    ) -> Result<ResolvedArtifactTargets> {
        let bills = self
            .bills_dir
            .as_ref()
            .or(config.bills_dir.as_ref())
            .cloned();
        let parcels = self
            .parcels_dir
            .as_ref()
            .or(config.parcels_dir.as_ref())
            .cloned();
        let subscriptions = self
            .subscriptions_dir
            .as_ref()
            .or(config.subscriptions_dir.as_ref())
            .cloned();

        let receipts = self.build_receipts_sink(config, runtime)?;
        let tickets = self.build_tickets_sink(config, runtime)?;

        Ok(ResolvedArtifactTargets {
            bills,
            parcels,
            subscriptions,
            receipts,
            tickets,
        })
    }

    /// Reconcile the three receipt sink variants (local dir, WebDAV,
    /// mail forwarder). All three are mutually exclusive; the CLI layer
    /// overrides the config layer when any CLI variant is set.
    fn build_receipts_sink(
        &self,
        config: &Config,
        runtime: &tokio::runtime::Handle,
    ) -> Result<Option<mailsift::targets::receipts::ReceiptSink>> {
        use mailsift::targets::receipts::ReceiptSink;

        let cli_local = self.receipts_dir.is_some();
        let cli_remote = self.receipts_webdav_url.is_some();
        let cli_forward = !self.receipts_forward_to.is_empty();
        let cli_count = cli_local as u8 + cli_remote as u8 + cli_forward as u8;
        if cli_count > 1 {
            anyhow::bail!(
                "specify at most one of --receipts-dir, --receipts-webdav-url, --receipts-forward-to"
            );
        }

        // CLI wins when any variant is set; otherwise fall back to
        // config.
        if cli_count == 1 {
            if let Some(dir) = &self.receipts_dir {
                return Ok(Some(ReceiptSink::LocalDir(dir.clone())));
            }
            if let Some(url) = &self.receipts_webdav_url {
                let sink = build_webdav_sink(
                    url,
                    self.receipts_webdav_password_file.clone(),
                    None, // no config block to fall back to
                    runtime,
                )?;
                return Ok(Some(ReceiptSink::Webdav(sink)));
            }
            let fwd = self.build_cli_forwarder(runtime)?;
            return Ok(Some(ReceiptSink::Forward(fwd)));
        }

        // Config fallback. Config::validate has ruled out conflicts.
        if let Some(dir) = &config.receipts_dir {
            return Ok(Some(ReceiptSink::LocalDir(dir.clone())));
        }
        if let Some(c) = config.receipts_webdav.as_ref() {
            let sink = build_webdav_sink(&c.url, None, Some(c), runtime)?;
            return Ok(Some(ReceiptSink::Webdav(sink)));
        }
        if let Some(cfg) = config.receipts_forward.as_ref() {
            let fwd = build_config_forwarder(cfg, runtime)?;
            return Ok(Some(ReceiptSink::Forward(fwd)));
        }
        Ok(None)
    }

    fn build_cli_forwarder(
        &self,
        runtime: &tokio::runtime::Handle,
    ) -> Result<mailsift::targets::mail_forward::MailForwarder> {
        let from = self.receipts_forward_from.as_deref().ok_or_else(|| {
            usage_error!("--receipts-forward-from is required with --receipts-forward-to")
        })?;

        let transport = match (
            self.receipts_forward_sendmail.as_ref(),
            self.receipts_forward_smtp_url.as_deref(),
        ) {
            (Some(_), Some(_)) => anyhow::bail!(
                "specify either --receipts-forward-sendmail or --receipts-forward-smtp-url, not both"
            ),
            (None, None) => anyhow::bail!(
                "--receipts-forward-to needs either --receipts-forward-sendmail or --receipts-forward-smtp-url"
            ),
            (Some(p), None) => ForwarderTransport::Sendmail(Some(p.clone())),
            (None, Some(url)) => ForwarderTransport::Smtp {
                url: url.to_string(),
                password_file: self.receipts_forward_smtp_password_file.clone(),
            },
        };

        build_forwarder(from, self.receipts_forward_to.clone(), transport, runtime)
    }

    /// Reconcile the local-dir and WebDAV flags + config sub-tables.
    /// Refuses to start when both are set on the same layer (mirroring
    /// the `--events-dir` / `--caldav-url` conflict for events).
    fn build_tickets_sink(
        &self,
        config: &Config,
        runtime: &tokio::runtime::Handle,
    ) -> Result<Option<mailsift::targets::tickets::TicketSink>> {
        use mailsift::targets::tickets::TicketSink;

        let cli_local = self.tickets_dir.clone();
        let cli_remote = self.tickets_webdav_url.clone();
        if cli_local.is_some() && cli_remote.is_some() {
            anyhow::bail!("specify either --tickets-dir or --tickets-webdav-url, not both");
        }

        // Config::validate has already ruled out the same conflict on
        // the config side, so we only need to pick a layer here.
        let from_cli = cli_local.is_some() || cli_remote.is_some();
        let (local, remote_url, cfg_block) = if from_cli {
            (cli_local, cli_remote, None)
        } else {
            (
                config.tickets_dir.clone(),
                config.tickets_webdav.as_ref().map(|c| c.url.clone()),
                config.tickets_webdav.as_ref(),
            )
        };

        if let Some(dir) = local {
            return Ok(Some(TicketSink::LocalDir(dir)));
        }
        let Some(url) = remote_url else {
            return Ok(None);
        };
        let sink = build_webdav_sink(
            &url,
            self.tickets_webdav_password_file.clone(),
            cfg_block,
            runtime,
        )?;
        Ok(Some(TicketSink::Webdav(sink)))
    }
}

struct ResolvedArtifactTargets {
    bills: Option<PathBuf>,
    parcels: Option<PathBuf>,
    subscriptions: Option<PathBuf>,
    receipts: Option<mailsift::targets::receipts::ReceiptSink>,
    tickets: Option<mailsift::targets::tickets::TicketSink>,
}

/// Helper for any WebDAV-flavoured sink construction (receipts or
/// tickets): parses the URL's userinfo, reads the password file, and
/// builds a [`WebdavSink`]. `cfg_fallback`, if supplied, provides
/// fallback values from a TOML config block.
fn build_webdav_sink(
    raw_url: &str,
    cli_password_file: Option<PathBuf>,
    cfg_fallback: Option<&mailsift::config::WebdavConfig>,
    runtime: &tokio::runtime::Handle,
) -> Result<mailsift::targets::webdav::WebdavSink> {
    use mailsift::targets::webdav::WebdavSink;

    let CaldavTarget {
        url,
        user: user_from_url,
    } = parse_caldav_url(raw_url)?;
    let user = user_from_url.or_else(|| cfg_fallback.and_then(|c| c.user.clone()));
    let password_file =
        cli_password_file.or_else(|| cfg_fallback.and_then(|c| c.password_file.clone()));
    let password = password_file.as_deref().map(read_secret_file).transpose()?;
    WebdavSink::new(url, user, password, runtime.clone())
}

/// Build a forwarder from a [`crate::config::ReceiptForwardConfig`].
/// Mirrors the CLI-driven [`ArtifactDirArgs::build_cli_forwarder`].
fn build_config_forwarder(
    cfg: &mailsift::config::ReceiptForwardConfig,
    runtime: &tokio::runtime::Handle,
) -> Result<mailsift::targets::mail_forward::MailForwarder> {
    // Config::validate has already ruled out both-or-neither, so this
    // dispatch is unambiguous.
    let transport = match (cfg.sendmail.as_ref(), cfg.smtp_url.as_deref()) {
        (Some(p), _) => ForwarderTransport::Sendmail(Some(p.clone())),
        (None, Some(url)) => ForwarderTransport::Smtp {
            url: url.to_string(),
            password_file: cfg.smtp_password_file.clone(),
        },
        (None, None) => unreachable!("Config::validate enforces one transport"),
    };
    build_forwarder(&cfg.from, cfg.to.clone(), transport, runtime)
}

/// Normalised view of "what transport does this forwarder use", shared
/// by the CLI and config paths. Both callers validate their layer and
/// hand a single transport here.
enum ForwarderTransport {
    Sendmail(Option<PathBuf>),
    #[cfg_attr(not(feature = "smtp"), allow(dead_code))]
    Smtp {
        url: String,
        password_file: Option<PathBuf>,
    },
}

fn build_forwarder(
    from: &str,
    to: Vec<String>,
    transport: ForwarderTransport,
    runtime: &tokio::runtime::Handle,
) -> Result<mailsift::targets::mail_forward::MailForwarder> {
    use mailsift::targets::mail_forward::MailForwarder;
    match transport {
        ForwarderTransport::Sendmail(path) => MailForwarder::sendmail(
            from,
            to,
            path.as_ref().map(|p| p.display().to_string()),
            runtime.clone(),
        ),
        #[cfg(feature = "smtp")]
        ForwarderTransport::Smtp { url, password_file } => {
            let password = password_file.as_deref().map(read_secret_file).transpose()?;
            MailForwarder::smtp(from, to, &url, password, runtime.clone())
        }
        #[cfg(not(feature = "smtp"))]
        ForwarderTransport::Smtp { .. } => {
            let _ = (from, to, runtime);
            anyhow::bail!("SMTP forwarding requires the `smtp` Cargo feature")
        }
    }
}

/// Resolve the directories to scan for extractor manifests.
///
/// CLI flag takes precedence over the config; the config value may
/// itself be a single path or a list. When nothing is configured we
/// fall back to the in-tree `extractors/` dir so the bundled defaults
/// keep working.
fn resolve_extractors(cli: Option<PathBuf>, config: &Config) -> Vec<PathBuf> {
    if let Some(path) = cli {
        return vec![path];
    }
    if let Some(dirs) = &config.extractors_dir {
        return dirs.as_slice().to_vec();
    }
    vec![PathBuf::from(DEFAULT_EXTRACTORS_DIR)]
}

/// Discover extractors for a processing command, failing if none were
/// found. A commonly-mistyped `extractors_dir` still names a real
/// directory, so discovery succeeds with an empty set; without this
/// guard the command would silently process nothing. `check` prints
/// its (possibly empty) result instead of calling this.
fn discover_required(dirs: &[PathBuf]) -> Result<Vec<mailsift::extractor::Extractor>> {
    let extractors = mailsift::extractor::discover(dirs)
        .with_context(|| format!("discovering extractors in {}", render_paths(dirs)))?;
    if extractors.is_empty() {
        return Err(anyhow!(
            "no extractors found under {}; check extractors_dir",
            render_paths(dirs)
        ));
    }
    tracing::info!(
        "loaded {} extractor{} from {}",
        extractors.len(),
        if extractors.len() == 1 { "" } else { "s" },
        render_paths(dirs),
    );
    Ok(extractors)
}

/// Parse a `YYYY-MM-DD` date into the local-midnight `SystemTime` we
/// compare Maildir mtimes against.
fn parse_since_date(s: &str) -> Result<std::time::SystemTime> {
    use chrono::{Local, NaiveDate, TimeZone};
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("parsing --since {s:?} (want YYYY-MM-DD)"))?;
    let dt = date.and_hms_opt(0, 0, 0).expect("00:00:00 is a valid time");
    let local = Local
        .from_local_datetime(&dt)
        .single()
        .ok_or_else(|| anyhow!("{s} is ambiguous or non-existent in local time"))?;
    Ok(local.into())
}

/// Print a per-extractor dispatch table from a `replay --explain` run.
/// Lists every discovered extractor (even those the pipeline never
/// considered because something earlier short-circuited) so the user
/// can see at a glance what matched and what didn't.
fn print_explain(
    extractors: &[mailsift::extractor::Extractor],
    explain: &[mailsift::pipeline::ExplainRecord],
) {
    use mailsift::pipeline::ExplainOutcome;
    use std::collections::HashMap;

    let by_name: HashMap<&str, &mailsift::pipeline::ExplainRecord> =
        explain.iter().map(|r| (r.extractor.as_str(), r)).collect();

    println!("\ndispatch:");
    for ex in extractors {
        let line = match by_name.get(ex.name.as_str()) {
            None => "skipped (earlier extractor short-circuited or never reached)".to_string(),
            Some(rec) => match &rec.outcome {
                ExplainOutcome::SkippedHeaders => "skipped: from/subject prefilter".to_string(),
                ExplainOutcome::SkippedBody => "skipped: body shape (`requires:`)".to_string(),
                ExplainOutcome::SkippedDkim => "skipped: DKIM (`require_dkim:`)".to_string(),
                ExplainOutcome::Failed { error } => format!("failed: {error}"),
                ExplainOutcome::Produced {
                    events,
                    reservations,
                    bills,
                    parcels,
                    receipts,
                    tickets,
                    subscriptions,
                } => {
                    let total = events
                        + reservations
                        + bills
                        + parcels
                        + receipts
                        + tickets
                        + subscriptions;
                    if total == 0 {
                        "matched, produced no artifacts".to_string()
                    } else {
                        let mut parts: Vec<String> = Vec::new();
                        for (n, label) in [
                            (*events, "event"),
                            (*reservations, "reservation"),
                            (*bills, "bill"),
                            (*parcels, "parcel"),
                            (*receipts, "receipt"),
                            (*tickets, "ticket"),
                            (*subscriptions, "subscription"),
                        ] {
                            if n > 0 {
                                let plural = if n == 1 { "" } else { "s" };
                                parts.push(format!("{n} {label}{plural}"));
                            }
                        }
                        format!("matched: produced {}", parts.join(", "))
                    }
                }
            },
        };
        println!("  {:<28}  {line}", ex.name);
    }
}

/// Print the `stats` subcommand's per-extractor summary table. One
/// row per extractor, sorted by [`mailsift::stats::aggregate`]
/// (most-run first), with totals + counts + mean wall-clock + the
/// last-seen `From:` domain. Skipped-* counts are folded into one
/// "skipped" column so the row stays readable; the breakdown is
/// available in the raw NDJSON log if anyone needs it.
fn print_stats(stats: &[mailsift::stats::ExtractorStats]) {
    if stats.is_empty() {
        println!("no events recorded");
        return;
    }
    println!(
        "{:<28} {:>6} {:>9} {:>6} {:>6} {:>7} {:>9}  last domain",
        "extractor", "runs", "produced", "empty", "failed", "skipped", "mean ms"
    );
    for s in stats {
        let mean = match s.mean_duration_ms {
            Some(m) => format!("{m:.0}"),
            None => "-".to_string(),
        };
        let last_domain = s.recent_domains.last().map(String::as_str).unwrap_or("-");
        let skipped = s.skipped_headers + s.skipped_body + s.skipped_dkim;
        println!(
            "{:<28} {:>6} {:>9} {:>6} {:>6} {:>7} {:>9}  {}",
            s.name, s.runs, s.produced, s.empty, s.failed, skipped, mean, last_domain
        );
    }
}

/// Render a slice of paths as a comma-separated string, for log /
/// error context lines.
fn render_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the per-process set of tracker-registration sinks. Each
/// sink-source (CLI flag or config section) contributes one element;
/// the returned value is empty when nothing is configured, which the
/// parcels target turns into a no-op.
fn build_trackers(
    cli: &TrackerArgs,
    config: &Config,
    runtime: &tokio::runtime::Handle,
) -> Result<mailsift::targets::trackers::Trackers> {
    use mailsift::targets::{karrio::KarrioClient, seventeentrack::SeventeenTrackClient};
    let mut trackers = mailsift::targets::trackers::Trackers::new();

    let karrio_url = cli
        .karrio_url
        .as_ref()
        .or_else(|| config.karrio.as_ref().map(|k| &k.url));
    let karrio_token_file = cli
        .karrio_token_file
        .as_ref()
        .or_else(|| config.karrio.as_ref().map(|k| &k.token_file));
    if let (Some(url), Some(token_file)) = (karrio_url, karrio_token_file) {
        let token = read_secret_file(token_file)?;
        trackers.push(KarrioClient::new(url.clone(), token, runtime.clone())?);
    }

    let st_token_file = cli
        .seventeentrack_token_file
        .as_ref()
        .or_else(|| config.seventeentrack.as_ref().map(|s| &s.token_file));
    if let Some(token_file) = st_token_file {
        let token = read_secret_file(token_file)?;
        trackers.push(SeventeenTrackClient::new(token, runtime.clone())?);
    }

    Ok(trackers)
}

/// Build a Firefly III sink from CLI + config. Returns `None` when
/// neither layer configures one, errors when the URL is set without a
/// token file (or vice versa).
fn build_firefly(
    cli: &FireflyArgs,
    config: &Config,
    runtime: &tokio::runtime::Handle,
) -> Result<Option<mailsift::targets::firefly::FireflySink>> {
    let url = cli
        .firefly_url
        .as_ref()
        .or_else(|| config.firefly.as_ref().map(|f| &f.url));
    let token_file = cli
        .firefly_token_file
        .as_ref()
        .or_else(|| config.firefly.as_ref().map(|f| &f.token_file));
    match (url, token_file) {
        (Some(url), Some(token_file)) => {
            let token = read_secret_file(token_file)?;
            Ok(Some(mailsift::targets::firefly::FireflySink::new(
                url.clone(),
                token,
                runtime.clone(),
            )?))
        }
        (None, None) => Ok(None),
        (Some(_), None) => {
            anyhow::bail!("--firefly-url (or [firefly].url) set without a token file")
        }
        (None, Some(_)) => {
            anyhow::bail!("--firefly-token-file (or [firefly].token_file) set without a URL")
        }
    }
}

fn main() -> Result<()> {
    match run() {
        // Caller mistakes (bad flags, a mailbox that doesn't exist) are not
        // bugs: a plain message is more useful than anyhow's Debug backtrace.
        // Everything else keeps the default `?`-from-main behaviour so real
        // failures stay loud.
        Err(err) if err.is::<UsageError>() || err.is::<imap_scan::MailboxNotFound>() => {
            eprintln!("error: {err}");
            std::process::exit(2);
        }
        other => other,
    }
}

fn run() -> Result<()> {
    // rustls 0.23 needs a CryptoProvider explicitly installed before any
    // TLS handshake; do it here so every subcommand that uses TLS
    // (imap-scan, milter health-checks, caldav target) is covered.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .with_target(false)
        .with_level(false)
        .init();

    let cli = Cli::parse();

    let config = match &cli.config {
        Some(path) => Config::load(path)?,
        None => match default_config_path() {
            Some(path) if path.exists() => Config::load(&path)?,
            _ => Config::default(),
        },
    };

    // Shared by every subcommand: the async HTTP targets block on it,
    // and the milter reuses it for its listener.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    match cli.command {
        Command::Check { extractors } => {
            let extractors_dir = resolve_extractors(extractors, &config);
            let discovered = mailsift::extractor::discover(&extractors_dir).with_context(|| {
                format!(
                    "discovering extractors in {}",
                    render_paths(&extractors_dir)
                )
            })?;
            println!(
                "found {} extractor(s) under {}",
                discovered.len(),
                render_paths(&extractors_dir)
            );
            for ex in &discovered {
                println!(
                    "  {order:>4}  {name:<28}  {script}",
                    order = ex.order,
                    name = ex.name,
                    script = ex.script.display(),
                );
            }
            Ok(())
        }
        Command::Lint { extractors } => {
            let extractors_dir = resolve_extractors(extractors, &config);
            let issues = mailsift::extractor::lint(&extractors_dir);
            if issues.is_empty() {
                println!("no issues found under {}", render_paths(&extractors_dir));
                Ok(())
            } else {
                for issue in &issues {
                    println!("{}: {}", issue.source.display(), issue.message);
                }
                let n = issues.len();
                let noun = if n == 1 { "issue" } else { "issues" };
                anyhow::bail!("{n} {noun} found")
            }
        }
        Command::Stats { log } => {
            let log_path = match log {
                Some(p) => p,
                None => match mailsift::stats::Recorder::default_file() {
                    mailsift::stats::Recorder::File(p) => p,
                    mailsift::stats::Recorder::Disabled => {
                        anyhow::bail!("no log path: set XDG_STATE_HOME or HOME, or pass --log")
                    }
                },
            };
            if !log_path.exists() {
                println!("no events recorded yet at {}", log_path.display());
                return Ok(());
            }
            let stats = mailsift::stats::aggregate(&log_path)?;
            print_stats(&stats);
            Ok(())
        }
        Command::Replay {
            path,
            extractors,
            target,
            artifacts,
            trackers,
            firefly,
            dry_run,
            explain,
        } => {
            let sink = target.build_sink(&config, runtime.handle())?;
            let extractors_dir = resolve_extractors(extractors, &config);
            let extractors = discover_required(&extractors_dir)?;
            let dirs = artifacts.resolve(&config, runtime.handle())?;
            let trackers = build_trackers(&trackers, &config, runtime.handle())?;
            let firefly = build_firefly(&firefly, &config, runtime.handle())?;
            let raw = if path == Path::new("-") {
                let mut buf = Vec::new();
                use std::io::Read;
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .context("reading stdin")?;
                buf
            } else {
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?
            };

            let source = if path == Path::new("-") {
                "replay stdin".to_string()
            } else {
                format!("replay {}", path.display())
            };
            let mut explain_buf: Vec<pipeline::ExplainRecord> = Vec::new();
            let result = pipeline::run(
                &raw,
                &source,
                &extractors,
                pipeline::PipelineTargets {
                    event_sink: &sink,
                    bills_dir: dirs.bills.as_deref(),
                    parcels_dir: dirs.parcels.as_deref(),
                    subscriptions_dir: dirs.subscriptions.as_deref(),
                    receipts: dirs.receipts.as_ref(),
                    tickets: dirs.tickets.as_ref(),
                    firefly: firefly.as_ref(),
                    trackers: (!trackers.is_empty()).then_some(&trackers),
                    trusted_forwarders: &config.trusted_forwarders,
                    // Replay is one-off debugging; don't pollute the
                    // long-running milter's stats log with it.
                    recorder: &mailsift::stats::Recorder::Disabled,
                    // Replay deliberately re-issues every upstream
                    // call so the user can see exactly what would
                    // happen. Dedup belongs to the daemon.
                    seen: None,
                },
                DkimPolicy::Enforce,
                dry_run,
                explain.then_some(&mut explain_buf),
            );
            if explain {
                print_explain(&extractors, &explain_buf);
            }
            result
        }
        Command::ImapScan(args) => {
            let ImapScanArgs {
                url,
                password_file,
                oauth2_token_file,
                oauth2_credentials_file,
                oauth2_refresh_token_file,
                oauth2_client_id,
                oauth2_client_secret_file,
                oauth2_provider,
                oauth2_token_endpoint,
                #[cfg_attr(not(feature = "gssapi"), allow(unused_variables))]
                authzid,
                since,
                limit,
                extractors,
                target,
                artifacts,
                trackers,
                firefly,
                dry_run,
                watch,
            } = *args;
            let sink = target.build_sink(&config, runtime.handle())?;
            let extractors_dir = resolve_extractors(extractors, &config);
            let extractors = discover_required(&extractors_dir)?;
            let dirs = artifacts.resolve(&config, runtime.handle())?;
            let trackers = build_trackers(&trackers, &config, runtime.handle())?;
            let firefly = build_firefly(&firefly, &config, runtime.handle())?;

            let target_imap = parse_imap_url(&url)?;
            // Read credentials up-front so their lifetimes outlive the
            // borrowed AuthMethod.
            let password = password_file.as_deref().map(read_secret_file).transpose()?;
            let oauth2_token = oauth2_token_file
                .as_deref()
                .map(read_secret_file)
                .transpose()?;
            // Both XOAUTH2 paths feed a `&dyn TokenSource`: a fixed token
            // wraps in a StaticTokenProvider, a refresh token (or a
            // credential bundle) in the minting TokenProvider. Bind both
            // here so they outlive the borrowed AuthMethod.
            let static_token_provider = oauth2_token
                .clone()
                .map(mailsift::oauth2::StaticTokenProvider::new);
            let refresh_token_provider = build_token_provider(
                &OAuth2ScanFlags {
                    credentials_file: oauth2_credentials_file.as_deref(),
                    refresh_token_file: oauth2_refresh_token_file.as_deref(),
                    client_id: oauth2_client_id.as_deref(),
                    client_secret_file: oauth2_client_secret_file.as_deref(),
                    provider_name: oauth2_provider.as_deref(),
                    token_endpoint: oauth2_token_endpoint.as_deref(),
                },
                &target_imap.host,
                runtime.handle(),
            )?;
            let xoauth2_tokens: Option<&dyn mailsift::oauth2::TokenSource> =
                match (&static_token_provider, &refresh_token_provider) {
                    (Some(s), _) => Some(s),
                    (None, Some(r)) => Some(r),
                    (None, None) => None,
                };
            let resolved_user = target_imap.user.clone().or_else(current_username);
            let auth_method = match (&password, xoauth2_tokens) {
                (Some(pw), _) => {
                    let user = resolved_user.as_deref().ok_or_else(|| {
                        usage_error!(
                            "no username in URL and could not look up the current user; \
                             include one in the URL (imaps://user@host/...)"
                        )
                    })?;
                    imap_scan::AuthMethod::Login { user, password: pw }
                }
                (None, Some(tokens)) => imap_scan::AuthMethod::XOAuth2 {
                    user: xoauth2_user(resolved_user.as_deref())?,
                    tokens,
                },
                #[cfg(feature = "gssapi")]
                (None, None) => imap_scan::AuthMethod::Gssapi {
                    authzid: authzid.as_deref(),
                },
                #[cfg(not(feature = "gssapi"))]
                (None, None) => {
                    anyhow::bail!(
                        "no --password-file, --oauth2-token-file or \
                         --oauth2-refresh-token-file given, and this build has no GSSAPI support"
                    )
                }
            };
            imap_scan::run(imap_scan::ImapScanConfig {
                host: &target_imap.host,
                port: target_imap.port,
                auth: auth_method,
                mailbox: &target_imap.mailbox,
                since: since.as_deref(),
                limit,
                extractors: &extractors,
                targets: pipeline::PipelineTargets {
                    event_sink: &sink,
                    bills_dir: dirs.bills.as_deref(),
                    parcels_dir: dirs.parcels.as_deref(),
                    subscriptions_dir: dirs.subscriptions.as_deref(),
                    receipts: dirs.receipts.as_ref(),
                    tickets: dirs.tickets.as_ref(),
                    firefly: firefly.as_ref(),
                    trackers: (!trackers.is_empty()).then_some(&trackers),
                    trusted_forwarders: &config.trusted_forwarders,
                    // Bulk import: don't skew the milter's stats, and
                    // bypass the dedup store so every event is re-PUT.
                    recorder: &mailsift::stats::Recorder::Disabled,
                    seen: None,
                },
                dry_run,
                watch,
            })
        }
        Command::ImapAuth(args) => run_imap_auth(args, runtime.handle()),
        Command::MaildirScan {
            path,
            recurse,
            since,
            limit,
            extractors,
            target,
            artifacts,
            trackers,
            firefly,
            dry_run,
        } => {
            let sink = target.build_sink(&config, runtime.handle())?;
            let extractors_dir = resolve_extractors(extractors, &config);
            let extractors = discover_required(&extractors_dir)?;
            let dirs = artifacts.resolve(&config, runtime.handle())?;
            let trackers = build_trackers(&trackers, &config, runtime.handle())?;
            let firefly = build_firefly(&firefly, &config, runtime.handle())?;
            let since = since.as_deref().map(parse_since_date).transpose()?;
            mailsift::maildir_scan::run(mailsift::maildir_scan::MaildirScanConfig {
                root: &path,
                recurse,
                since,
                limit,
                extractors: &extractors,
                targets: pipeline::PipelineTargets {
                    event_sink: &sink,
                    bills_dir: dirs.bills.as_deref(),
                    parcels_dir: dirs.parcels.as_deref(),
                    subscriptions_dir: dirs.subscriptions.as_deref(),
                    receipts: dirs.receipts.as_ref(),
                    tickets: dirs.tickets.as_ref(),
                    firefly: firefly.as_ref(),
                    trackers: (!trackers.is_empty()).then_some(&trackers),
                    trusted_forwarders: &config.trusted_forwarders,
                    // Bulk import: mirror imap-scan's stance (don't
                    // pollute the milter's stats, don't use its dedup
                    // store; upstream sinks are already idempotent).
                    recorder: &mailsift::stats::Recorder::Disabled,
                    seen: None,
                },
                dry_run,
            })
        }
        #[cfg(feature = "web")]
        Command::Web {
            listen,
            socket_mode,
            base_path,
        } => {
            let listen = mailsift::web::Listen::parse(&listen)?;
            let base_path = mailsift::web::normalise_base_path(&base_path);
            let config = std::sync::Arc::new(config);
            runtime.block_on(mailsift::web::serve(listen, config, base_path, socket_mode))
        }
        Command::Milter {
            socket,
            extractors,
            target,
            artifacts,
            trackers,
            firefly,
            deadline_secs,
        } => {
            let sink = target.build_sink(&config, runtime.handle())?;
            let extractors_dir = resolve_extractors(extractors, &config);
            let extractors = discover_required(&extractors_dir)?;
            let dirs = artifacts.resolve(&config, runtime.handle())?;
            let tickets_sink = dirs.tickets.map(std::sync::Arc::new);
            let trackers = build_trackers(&trackers, &config, runtime.handle())?;
            let firefly_sink =
                build_firefly(&firefly, &config, runtime.handle())?.map(std::sync::Arc::new);
            let receipts_sink = dirs.receipts.map(std::sync::Arc::new);
            let config = milter::MilterConfig {
                extractors: std::sync::Arc::new(extractors),
                targets: pipeline::OwnedTargets {
                    event_sink: std::sync::Arc::new(sink),
                    bills_dir: dirs.bills,
                    parcels_dir: dirs.parcels,
                    subscriptions_dir: dirs.subscriptions,
                    receipts: receipts_sink,
                    tickets: tickets_sink,
                    firefly: firefly_sink,
                    trackers: (!trackers.is_empty()).then(|| std::sync::Arc::new(trackers)),
                    trusted_forwarders: config.trusted_forwarders.clone(),
                    // Long-running daemon: record every extractor
                    // decision so `mailsift stats` has useful data.
                    recorder: mailsift::stats::Recorder::default_file(),
                    // Open seen.db best-effort; if it can't be
                    // opened we warn and fall back to no dedup
                    // (server-side replace handles correctness, we
                    // just pay the network cost).
                    seen: mailsift::seen::Store::default_path().and_then(|p| {
                        match mailsift::seen::Store::open(&p) {
                            Ok(s) => Some(s),
                            Err(e) => {
                                tracing::warn!(
                                    error = format!("{e:#}"),
                                    path = %p.display(),
                                    "seen.db open failed; running without dedup"
                                );
                                None
                            }
                        }
                    }),
                },
                deadline: Duration::from_secs(deadline_secs),
            };
            runtime.block_on(milter::run(&socket, config))
        }
    }
}
