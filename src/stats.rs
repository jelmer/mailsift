//! Per-extractor stats: an append-only NDJSON event log written from
//! the pipeline, and an aggregator that turns the log into a summary
//! table for the `stats` subcommand.
//!
//! ## Why NDJSON
//!
//! The milter handles messages concurrently. A single JSON file with
//! read-modify-write semantics would either lose updates under
//! concurrency or need a mutex on the hot path. NDJSON sidesteps both:
//! a `write(2)` smaller than `PIPE_BUF` (4 KiB on Linux) with
//! `O_APPEND` is atomic per POSIX, so any number of concurrent writers
//! can append safely. Aggregation happens once, at read time.
//!
//! ## Where the file lives
//!
//! `$XDG_STATE_HOME/mailsift/events.ndjson`, falling back to
//! `~/.local/state/mailsift/events.ndjson`. The directory is created
//! on first write. To disable recording entirely, pass [`Recorder::Disabled`].

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Outcome label written to the log for one extractor run on one
/// message. Mirrors [`crate::pipeline::ExplainOutcome`] but flattened
/// into a string so the JSON schema is stable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Extractor ran and produced at least one artifact.
    Produced,
    /// Extractor ran but produced no artifacts (matched the message
    /// but found nothing to extract).
    Empty,
    /// Extractor was forked but its process failed (non-zero exit or
    /// timeout).
    Failed,
    /// Prefilter (`from_domains` / `subject_regex`) ruled it out.
    SkippedHeaders,
    /// `requires:` body shape didn't match.
    SkippedBody,
    /// `require_dkim` wasn't satisfied.
    SkippedDkim,
}

/// One event log line. Kept narrow on purpose: storing full per-message
/// metadata would balloon the log and creep into the user's mail
/// content; only fields useful for "which extractors are pulling their
/// weight" go in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Unix epoch seconds when the run completed.
    pub ts: i64,
    pub extractor: String,
    pub outcome: Outcome,
    /// Wall-clock run time in milliseconds. Only populated for
    /// outcomes that actually forked the extractor (`Produced`,
    /// `Empty`, `Failed`). `None` for the `Skipped*` outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Lowercased `From:` domain. Useful for grouping the long tail
    /// of extractors that match more than one sender domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_domain: Option<String>,
}

/// Where (if anywhere) per-run events get appended. Wired into
/// [`crate::pipeline::PipelineTargets`] so every callpath that runs
/// extractors gets the same recorder.
#[derive(Clone)]
pub enum Recorder {
    /// No-op: skip recording entirely. Used in tests and when the user
    /// has explicitly turned recording off.
    Disabled,
    /// Append events to this file. The file is opened in append mode
    /// on each write so the log survives `mailsift` restarts without
    /// any file-handle bookkeeping.
    File(PathBuf),
}

impl Recorder {
    /// Default recorder, writing to `$XDG_STATE_HOME/mailsift/events.ndjson`
    /// (or `~/.local/state/mailsift/events.ndjson`). Returns
    /// [`Recorder::Disabled`] if neither `$XDG_STATE_HOME` nor `$HOME`
    /// is set; a stripped daemon environment shouldn't crash here.
    pub fn default_file() -> Self {
        match default_log_path() {
            Some(p) => Recorder::File(p),
            None => Recorder::Disabled,
        }
    }

    /// Append one event. Errors are logged but never propagated:
    /// dropping a stats record mustn't break extraction.
    pub fn record(&self, event: &Event) {
        let Recorder::File(path) = self else { return };
        if let Err(e) = append_event(path, event) {
            warn!(
                error = format!("{e:#}"),
                path = %path.display(),
                "failed to record stats event"
            );
        }
    }
}

/// Default log path: `$XDG_STATE_HOME/mailsift/events.ndjson` or
/// `$HOME/.local/state/mailsift/events.ndjson`.
fn default_log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
        })?;
    Some(base.join("mailsift").join("events.ndjson"))
}

/// Open the file in append+create mode and write one JSON line.
/// `O_APPEND` makes the `write(2)` atomic for any payload smaller than
/// `PIPE_BUF`; our lines are well under 4 KiB, so concurrent milter
/// tasks can append without coordination.
fn append_event(path: &Path, event: &Event) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut line = serde_json::to_string(event).context("serialising event")?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// Aggregated counts for one extractor, computed by [`aggregate`].
#[derive(Debug, Default, Serialize)]
pub struct ExtractorStats {
    pub name: String,
    pub runs: u64,
    pub produced: u64,
    pub empty: u64,
    pub failed: u64,
    pub skipped_headers: u64,
    pub skipped_body: u64,
    pub skipped_dkim: u64,
    /// Mean wall-clock runtime in milliseconds across runs that
    /// actually forked the extractor (`produced + empty + failed`).
    /// `None` when there were no such runs.
    pub mean_duration_ms: Option<f64>,
    /// Last (most recent) `ts` we saw for this extractor.
    pub last_ts: Option<i64>,
    /// Up to N most recent distinct `From:` domains for this
    /// extractor, oldest-first. Bounded to keep the table compact.
    pub recent_domains: Vec<String>,
}

const RECENT_DOMAINS_KEEP: usize = 3;

/// Read every event from `path` and reduce to one [`ExtractorStats`]
/// per extractor. Returns the aggregates sorted by `runs` descending
/// (so the most-active extractors appear first).
pub fn aggregate(path: &Path) -> Result<Vec<ExtractorStats>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut by_name: BTreeMap<String, Acc> = BTreeMap::new();
    let mut parse_errors = 0u64;

    for (lineno, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading line {}", lineno + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Event = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        by_name
            .entry(event.extractor.clone())
            .or_default()
            .bump(&event);
    }

    if parse_errors > 0 {
        warn!(
            count = parse_errors,
            "skipped malformed event lines while aggregating stats"
        );
    }

    let mut stats: Vec<ExtractorStats> = by_name
        .into_iter()
        .map(|(name, acc)| acc.into_stats(name))
        .collect();
    stats.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.name.cmp(&b.name)));
    Ok(stats)
}

/// Running sums + capped recent-domains list, one per extractor.
/// Kept private; only [`aggregate`] uses it as an intermediate.
#[derive(Default)]
struct Acc {
    runs: u64,
    produced: u64,
    empty: u64,
    failed: u64,
    skipped_headers: u64,
    skipped_body: u64,
    skipped_dkim: u64,
    /// Sum of `duration_ms` for runs that have one. Paired with
    /// `forked` so the mean is `total_ms / forked`.
    total_ms: u64,
    forked: u64,
    last_ts: Option<i64>,
    recent_domains: Vec<String>,
}

impl Acc {
    fn bump(&mut self, event: &Event) {
        self.runs += 1;
        match event.outcome {
            Outcome::Produced => self.produced += 1,
            Outcome::Empty => self.empty += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::SkippedHeaders => self.skipped_headers += 1,
            Outcome::SkippedBody => self.skipped_body += 1,
            Outcome::SkippedDkim => self.skipped_dkim += 1,
        }
        if let Some(ms) = event.duration_ms {
            self.total_ms += ms;
            self.forked += 1;
        }
        self.last_ts = Some(self.last_ts.map_or(event.ts, |prev| prev.max(event.ts)));
        if let Some(domain) = event.from_domain.as_deref() {
            // Keep the list in append-order, drop the oldest when we
            // exceed the cap. De-dupe so a chatty single sender
            // doesn't push every slot to one domain.
            if !self.recent_domains.iter().any(|d| d == domain) {
                self.recent_domains.push(domain.to_string());
                if self.recent_domains.len() > RECENT_DOMAINS_KEEP {
                    self.recent_domains.remove(0);
                }
            }
        }
    }

    fn into_stats(self, name: String) -> ExtractorStats {
        let mean = (self.forked > 0).then(|| self.total_ms as f64 / self.forked as f64);
        ExtractorStats {
            name,
            runs: self.runs,
            produced: self.produced,
            empty: self.empty,
            failed: self.failed,
            skipped_headers: self.skipped_headers,
            skipped_body: self.skipped_body,
            skipped_dkim: self.skipped_dkim,
            mean_duration_ms: mean,
            last_ts: self.last_ts,
            recent_domains: self.recent_domains,
        }
    }
}

/// Convenience: convert a [`std::time::Duration`] to whole
/// milliseconds for the on-disk `duration_ms` field. Saturating cast
/// because we never expect a single extractor to run for >68 years.
pub fn duration_to_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_events(events: &[Event]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("events.ndjson");
        for event in events {
            append_event(&path, event).unwrap();
        }
        (dir, path)
    }

    fn event(extractor: &str, outcome: Outcome, ts: i64, duration_ms: Option<u64>) -> Event {
        Event {
            ts,
            extractor: extractor.into(),
            outcome,
            duration_ms,
            from_domain: None,
        }
    }

    #[test]
    fn aggregate_one_extractor_one_run() {
        let (_d, path) = write_events(&[event("ns", Outcome::Produced, 1, Some(120))]);
        let stats = aggregate(&path).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "ns");
        assert_eq!(stats[0].runs, 1);
        assert_eq!(stats[0].produced, 1);
        assert_eq!(stats[0].mean_duration_ms, Some(120.0));
        assert_eq!(stats[0].last_ts, Some(1));
    }

    #[test]
    fn aggregate_sorts_by_runs_descending() {
        let (_d, path) = write_events(&[
            event("a", Outcome::Produced, 1, Some(50)),
            event("b", Outcome::Produced, 2, Some(80)),
            event("b", Outcome::Produced, 3, Some(70)),
        ]);
        let stats = aggregate(&path).unwrap();
        assert_eq!(stats[0].name, "b");
        assert_eq!(stats[0].runs, 2);
        assert_eq!(stats[1].name, "a");
        assert_eq!(stats[1].runs, 1);
    }

    #[test]
    fn aggregate_means_only_forked_runs() {
        // `SkippedHeaders` has no duration_ms; it must not pull the
        // mean down to zero.
        let (_d, path) = write_events(&[
            event("ns", Outcome::Produced, 1, Some(100)),
            event("ns", Outcome::SkippedHeaders, 2, None),
        ]);
        let stats = aggregate(&path).unwrap();
        assert_eq!(stats[0].mean_duration_ms, Some(100.0));
        assert_eq!(stats[0].runs, 2);
        assert_eq!(stats[0].skipped_headers, 1);
    }

    #[test]
    fn aggregate_tracks_recent_domains_dedup_and_cap() {
        let mut events = Vec::new();
        for (i, dom) in [
            "ns.nl",
            "ns.nl", // duplicate; should not push other slots
            "easyjet.com",
            "klm.com",
            "airfrance.fr", // pushes ns.nl out (cap = 3)
        ]
        .iter()
        .enumerate()
        {
            events.push(Event {
                ts: i as i64,
                extractor: "x".into(),
                outcome: Outcome::Produced,
                duration_ms: Some(10),
                from_domain: Some((*dom).into()),
            });
        }
        let (_d, path) = write_events(&events);
        let stats = aggregate(&path).unwrap();
        assert_eq!(
            stats[0].recent_domains,
            vec!["easyjet.com", "klm.com", "airfrance.fr"]
        );
    }

    #[test]
    fn aggregate_skips_malformed_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("events.ndjson");
        // Write a good line, a junk line, and another good line.
        let good1 = serde_json::to_string(&event("ns", Outcome::Produced, 1, Some(10))).unwrap();
        let good2 = serde_json::to_string(&event("ns", Outcome::Failed, 2, Some(20))).unwrap();
        std::fs::write(&path, format!("{good1}\nnot json at all\n{good2}\n")).unwrap();
        let stats = aggregate(&path).unwrap();
        assert_eq!(stats[0].runs, 2);
        assert_eq!(stats[0].produced, 1);
        assert_eq!(stats[0].failed, 1);
    }

    #[test]
    fn recorder_disabled_is_a_noop() {
        // Calling record() on a Disabled recorder mustn't touch disk
        // or panic.
        let r = Recorder::Disabled;
        r.record(&event("ns", Outcome::Produced, 1, Some(1)));
    }

    #[test]
    fn recorder_file_appends_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("events.ndjson");
        let r = Recorder::File(path.clone());
        r.record(&event("a", Outcome::Produced, 1, Some(10)));
        r.record(&event("a", Outcome::Failed, 2, Some(20)));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }

    #[test]
    fn default_log_path_uses_xdg_state_home() {
        // SAFETY: env mutations are not thread-safe; tests in this
        // crate are run serially (libtest default with one thread per
        // process).
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state");
            std::env::set_var("HOME", "/tmp/home");
        }
        assert_eq!(
            default_log_path(),
            Some(PathBuf::from("/tmp/xdg-state/mailsift/events.ndjson"))
        );

        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
        assert_eq!(
            default_log_path(),
            Some(PathBuf::from(
                "/tmp/home/.local/state/mailsift/events.ndjson"
            ))
        );

        unsafe {
            if let Some(v) = prev_state {
                std::env::set_var("XDG_STATE_HOME", v);
            }
            if let Some(v) = prev_home {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    #[test]
    fn duration_ms_saturates_on_overflow() {
        assert_eq!(duration_to_ms(Duration::from_secs(5)), 5_000);
        // Practically unreachable but the saturating cast must hold.
        assert_eq!(duration_to_ms(Duration::from_secs(u64::MAX)), u64::MAX);
    }
}
