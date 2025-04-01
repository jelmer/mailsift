use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use mailsift::pipeline;
use mailsift::targets::EventSinkKind;

#[derive(Parser)]
#[command(name = "mailsift", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
        /// File events as `<UID>.ics` under this directory.
        #[arg(long)]
        events_dir: PathBuf,
        /// Don't actually file artifacts; just report what would happen.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Replay {
            path,
            extractors,
            events_dir,
            dry_run,
        } => {
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

            let sink = EventSinkKind::LocalDir(events_dir);
            let source = if path == Path::new("-") {
                "replay stdin".to_string()
            } else {
                format!("replay {}", path.display())
            };
            pipeline::run(&raw, &source, &extractors, &sink, dry_run)
        }
    }
}
