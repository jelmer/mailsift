use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::artifacts::{Artifact, Kind};
use crate::extractor;
use crate::targets::{EventSink, EventSinkKind, FileOutcome, split_calendar};

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
    _dry_run: bool,
) -> Result<()> {
    let extractors = extractor::discover(extractors_dir)
        .with_context(|| format!("discovering extractors in {}", extractors_dir.display()))?;
    if extractors.is_empty() {
        warn!("no extractors configured; nothing to do");
        return Ok(());
    }

    debug!(count = extractors.len(), "running extractors");

    let mut filed = 0usize;

    for ex in &extractors {
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
            }
        }
    }

    if filed > 0 {
        info!("extracted from {source}: {filed} event(s)");
    }

    Ok(())
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
