use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};

use mailsift::cli::{CaldavTarget, parse_caldav_url};
use mailsift::pipeline::{self, DkimPolicy};
use mailsift::targets::{EventSinkKind, caldav};

/// Read a password / API-token file, trim, and return its contents.
fn read_secret_file(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_string())
}

#[derive(Parser)]
#[command(name = "mailsift", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where to send `event` artifacts. Either a local directory or a CalDAV
/// collection; exactly one must be set.
#[derive(Args, Clone, Default)]
struct EventTargetArgs {
    /// File events as `<UID>.ics` under this directory.
    #[arg(long)]
    events_dir: Option<PathBuf>,
    /// PUT events to this CalDAV server. The sink runs PROPFIND from
    /// this URL to find the user's schedule inbox and default
    /// calendar; the server root is usually enough. May include a
    /// username (`https://user@host/`); the password (if any) comes
    /// from `--caldav-password-file`.
    #[arg(long)]
    caldav_url: Option<String>,
    /// File containing the CalDAV password.
    #[arg(long)]
    caldav_password_file: Option<PathBuf>,
}

impl EventTargetArgs {
    fn build_sink(&self, runtime: &tokio::runtime::Handle) -> Result<EventSinkKind> {
        match (&self.events_dir, &self.caldav_url) {
            (Some(_), Some(_)) => Err(anyhow!(
                "specify either --events-dir or a CalDAV target, not both"
            )),
            (Some(dir), None) => Ok(EventSinkKind::LocalDir(dir.clone())),
            (None, Some(raw_url)) => {
                let CaldavTarget { url, user } = parse_caldav_url(raw_url)?;
                let password = self
                    .caldav_password_file
                    .as_deref()
                    .map(read_secret_file)
                    .transpose()?;
                Ok(EventSinkKind::Caldav(caldav::CaldavSink::new(
                    url,
                    user,
                    password,
                    runtime.clone(),
                )?))
            }
            (None, None) => Err(anyhow!(
                "no event target specified: pass --events-dir or --caldav-url"
            )),
        }
    }
}

/// Firefly III bill-registration flags. When both are set, every
/// filed bill is also registered with Firefly III.
#[derive(Args, Clone, Default)]
struct FireflyArgs {
    /// Base URL of a Firefly III instance.
    #[arg(long)]
    firefly_url: Option<String>,
    /// File containing a Firefly Personal Access Token.
    #[arg(long)]
    firefly_token_file: Option<PathBuf>,
}

/// Parcel-tracker-registration flags.
#[derive(Args, Clone, Default)]
struct TrackerArgs {
    /// Base URL of a Karrio instance.
    #[arg(long)]
    karrio_url: Option<String>,
    /// File containing the Karrio API token.
    #[arg(long)]
    karrio_token_file: Option<PathBuf>,
    /// File containing a 17track API token.
    #[arg(long)]
    seventeentrack_token_file: Option<PathBuf>,
}

fn build_trackers(
    cli: &TrackerArgs,
    runtime: &tokio::runtime::Handle,
) -> Result<mailsift::targets::trackers::Trackers> {
    use mailsift::targets::{karrio::KarrioClient, seventeentrack::SeventeenTrackClient};
    let mut trackers = mailsift::targets::trackers::Trackers::new();
    if let (Some(url), Some(token_file)) = (&cli.karrio_url, &cli.karrio_token_file) {
        let token = read_secret_file(token_file)?;
        trackers.push(KarrioClient::new(url.clone(), token, runtime.clone())?);
    }
    if let Some(token_file) = &cli.seventeentrack_token_file {
        let token = read_secret_file(token_file)?;
        trackers.push(SeventeenTrackClient::new(token, runtime.clone())?);
    }
    Ok(trackers)
}

#[derive(Subcommand)]
enum Command {
    /// Run the pipeline against a saved .eml file.
    Replay {
        /// Path to the .eml file. Use "-" for stdin.
        path: PathBuf,
        /// Directory containing extractor scripts.
        #[arg(long)]
        extractors: PathBuf,
        #[command(flatten)]
        target: EventTargetArgs,
        /// Directory under which to file `bill` artifacts.
        #[arg(long)]
        bills_dir: Option<PathBuf>,
        /// Directory under which to file `parcel` artifacts.
        #[arg(long)]
        parcels_dir: Option<PathBuf>,
        /// Directory under which to file `receipt` artifacts. Mutually
        /// exclusive with `--receipts-webdav-url` and
        /// `--receipts-forward-to`.
        #[arg(long)]
        receipts_dir: Option<PathBuf>,
        /// WebDAV collection URL to PUT `receipt` artifacts to.
        #[arg(long)]
        receipts_webdav_url: Option<String>,
        /// Password file for the receipts WebDAV target.
        #[arg(long)]
        receipts_webdav_password_file: Option<PathBuf>,
        /// Mailbox to forward receipt-emitting messages to.
        #[arg(long, value_delimiter = ',')]
        receipts_forward_to: Vec<String>,
        /// `From:` mailbox to put on forwarded receipt mails.
        #[arg(long)]
        receipts_forward_from: Option<String>,
        /// Path to a sendmail binary.
        #[arg(long)]
        receipts_forward_sendmail: Option<PathBuf>,
        #[command(flatten)]
        firefly: FireflyArgs,
        #[command(flatten)]
        trackers: TrackerArgs,
        /// Don't actually file artifacts; just report what would happen.
        #[arg(long)]
        dry_run: bool,
    },
}

fn build_receipts_sink(
    receipts_dir: Option<PathBuf>,
    webdav_url: Option<String>,
    webdav_password_file: Option<PathBuf>,
    forward_to: Vec<String>,
    forward_from: Option<String>,
    forward_sendmail: Option<PathBuf>,
    runtime: &tokio::runtime::Handle,
) -> Result<Option<mailsift::targets::receipts::ReceiptSink>> {
    use mailsift::targets::receipts::ReceiptSink;
    let count =
        receipts_dir.is_some() as u8 + webdav_url.is_some() as u8 + (!forward_to.is_empty()) as u8;
    if count > 1 {
        anyhow::bail!(
            "specify at most one of --receipts-dir, --receipts-webdav-url, --receipts-forward-to"
        );
    }
    if let Some(dir) = receipts_dir {
        return Ok(Some(ReceiptSink::LocalDir(dir)));
    }
    if let Some(url) = webdav_url {
        let CaldavTarget { url, user } = parse_caldav_url(&url)?;
        let password = webdav_password_file
            .as_deref()
            .map(read_secret_file)
            .transpose()?;
        let sink =
            mailsift::targets::webdav::WebdavSink::new(url, user, password, runtime.clone())?;
        return Ok(Some(ReceiptSink::Webdav(sink)));
    }
    if !forward_to.is_empty() {
        let from = forward_from.ok_or_else(|| {
            anyhow!("--receipts-forward-from required with --receipts-forward-to")
        })?;
        let fwd = mailsift::targets::mail_forward::MailForwarder::sendmail(
            &from,
            forward_to,
            forward_sendmail.as_ref().map(|p| p.display().to_string()),
            runtime.clone(),
        )?;
        return Ok(Some(ReceiptSink::Forward(fwd)));
    }
    Ok(None)
}

fn build_firefly(
    cli: &FireflyArgs,
    runtime: &tokio::runtime::Handle,
) -> Result<Option<mailsift::targets::firefly::FireflySink>> {
    match (&cli.firefly_url, &cli.firefly_token_file) {
        (Some(url), Some(token_file)) => {
            let token = read_secret_file(token_file)?;
            Ok(Some(mailsift::targets::firefly::FireflySink::new(
                url.clone(),
                token,
                runtime.clone(),
            )?))
        }
        (None, None) => Ok(None),
        (Some(_), None) => anyhow::bail!("--firefly-url set without a token file"),
        (None, Some(_)) => anyhow::bail!("--firefly-token-file set without a URL"),
    }
}

fn main() -> Result<()> {
    // rustls 0.23 needs a CryptoProvider explicitly installed before any
    // TLS handshake; do it here so every subcommand that uses TLS
    // (caldav target today, more landing later) is covered.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .with_target(false)
        .init();

    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    match cli.command {
        Command::Replay {
            path,
            extractors,
            target,
            bills_dir,
            parcels_dir,
            receipts_dir,
            receipts_webdav_url,
            receipts_webdav_password_file,
            receipts_forward_to,
            receipts_forward_from,
            receipts_forward_sendmail,
            firefly,
            trackers,
            dry_run,
        } => {
            let sink = target.build_sink(runtime.handle())?;
            let firefly = build_firefly(&firefly, runtime.handle())?;
            let trackers = build_trackers(&trackers, runtime.handle())?;
            let receipts = build_receipts_sink(
                receipts_dir,
                receipts_webdav_url,
                receipts_webdav_password_file,
                receipts_forward_to,
                receipts_forward_from,
                receipts_forward_sendmail,
                runtime.handle(),
            )?;
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
            pipeline::run(
                &raw,
                &source,
                &extractors,
                &sink,
                bills_dir.as_deref(),
                parcels_dir.as_deref(),
                receipts.as_ref(),
                firefly.as_ref(),
                (!trackers.is_empty()).then_some(&trackers),
                &[],
                DkimPolicy::Enforce,
                dry_run,
            )
        }
    }
}
