//! `mailsift maildir-scan`: walk a Maildir on disk and run the pipeline
//! over each message.
//!
//! Useful for one-off backfills against archived mail: fed messages
//! directly off disk, no MTA in the loop, no IMAP round trips. Mirrors
//! `imap-scan`'s pipeline contract (bypasses the milter's dedup store
//! and stats recorder; CalDAV etc. are idempotent on their own, and we
//! don't want bulk imports polluting the daemon's records).
//!
//! Maildir layout is per D. J. Bernstein: `cur/` and `new/` hold
//! delivered messages, `tmp/` is scratch space that MUAs may write to
//! mid-delivery and we ignore. With `recurse`, we also descend into
//! subfolders in the Maildir++ style (subdirectories whose name starts
//! with `.`, each with its own `cur/`+`new/`). Non-Maildir directories
//! are skipped with a warning.
//!
//! Ordering is deterministic (paths sorted) so a `--limit`ed run is
//! reproducible.
//!
//! When every selected extractor declares `from_domains` /
//! `subject_regex` hints (typically after narrowing the set with
//! `--extractor`), we read only each file's header block and drop
//! messages no extractor could match before reading the body. This is
//! the on-disk analogue of `imap-scan`'s `BODY[HEADER.FIELDS]`
//! prefilter: same [`Extractor::matches_headers`] decision, just fed
//! from a partial file read instead of a partial FETCH.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::extractor::Extractor;
use crate::pipeline::{self, PipelineTargets};

/// How many bytes to read from the head of a message file when
/// prefiltering on `From`/`Subject`. Generously above any realistic
/// header block, so in practice the terminating blank line is always
/// within the window; when it isn't we fall back to scanning the
/// message rather than guessing.
const HEADER_PEEK_BYTES: usize = 64 * 1024;

pub struct MaildirScanConfig<'a> {
    /// Root Maildir path (contains `cur/`, `new/`, `tmp/`).
    pub root: &'a Path,
    /// Also process Maildir++ subfolders (`.name/cur`, `.name/new`) recursively.
    pub recurse: bool,
    /// Skip messages whose file mtime is older than this. `None` means
    /// no lower bound.
    pub since: Option<SystemTime>,
    /// Cap on messages processed. Applied after enumeration so the cap
    /// is against the same ordered list every run.
    pub limit: Option<usize>,
    pub extractors: &'a [crate::extractor::Extractor],
    pub targets: PipelineTargets<'a>,
    pub dry_run: bool,
}

/// Enumerate Maildir folders under `root`. A folder is a
/// `(cur, new)` pair; `tmp` is always ignored.
fn discover_folders(root: &Path, recurse: bool) -> Result<Vec<PathBuf>> {
    let mut folders = Vec::new();
    let cur = root.join("cur");
    let new = root.join("new");
    if !cur.is_dir() || !new.is_dir() {
        anyhow::bail!(
            "{} does not look like a Maildir (missing cur/ or new/)",
            root.display()
        );
    }
    folders.push(root.to_path_buf());

    if recurse {
        for entry in
            std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))?
        {
            let entry = entry.with_context(|| format!("reading entry in {}", root.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.') || name.as_ref() == "." || name.as_ref() == ".." {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.join("cur").is_dir() && path.join("new").is_dir() {
                folders.push(path);
            } else {
                debug!(path = %path.display(), "skipping non-Maildir dotdir");
            }
        }
    }
    folders.sort();
    Ok(folders)
}

/// Collect message file paths from a single Maildir's `cur/` and `new/`.
fn collect_messages(folder: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for sub in ["cur", "new"] {
        let dir = folder.join(sub);
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
            let ft = entry
                .file_type()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            if ft.is_file() {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Read the header block from the head of a message file.
///
/// Returns `None` when the blank line separating headers from body
/// isn't within [`HEADER_PEEK_BYTES`], which means we can't be sure
/// we've seen every header and must not prefilter on a partial view.
/// A short file that ends before any blank line is all headers and no
/// body, so its full contents are returned.
fn read_header_block(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0u8; HEADER_PEEK_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        let n = file
            .read(&mut buf[filled..])
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    if filled < HEADER_PEEK_BYTES {
        // Hit EOF: the whole message is in `buf`, headers included.
        return Ok(Some(buf));
    }
    if find_header_end(&buf).is_some() {
        Ok(Some(buf))
    } else {
        Ok(None)
    }
}

/// Offset just past the blank line that ends the header block, for
/// either CRLF or bare-LF line endings.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Decide, from a message file's headers alone, whether any of
/// `extractors` could match it.
///
/// Errs on the side of keeping the message: an unreadable file, or one
/// whose header block doesn't fit the peek window, is passed through
/// to the pipeline, which will report the problem properly.
fn could_match_any(path: &Path, extractors: &[Extractor]) -> bool {
    let header = match read_header_block(path) {
        Ok(Some(h)) => h,
        Ok(None) => {
            debug!(
                path = %path.display(),
                "header block exceeds peek window; skipping prefilter"
            );
            return true;
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "header read failed; keeping file");
            return true;
        }
    };
    let (from_domain, subject) = pipeline::match_headers_from_raw(&header);
    extractors
        .iter()
        .any(|ex| ex.matches_headers(from_domain.as_deref(), subject.as_deref()))
}

pub fn run(config: MaildirScanConfig<'_>) -> Result<()> {
    let folders = discover_folders(config.root, config.recurse)?;
    info!(
        root = %config.root.display(),
        folders = folders.len(),
        "enumerated Maildir folders"
    );

    let mut all: Vec<PathBuf> = Vec::new();
    for folder in &folders {
        let mut msgs = collect_messages(folder)?;
        all.append(&mut msgs);
    }

    if let Some(cutoff) = config.since {
        all.retain(|p| match std::fs::metadata(p).and_then(|m| m.modified()) {
            Ok(mtime) => mtime >= cutoff,
            Err(e) => {
                warn!(path = %p.display(), error = %e, "stat failed; keeping file");
                true
            }
        });
    }

    // Prefilter on headers only when every selected extractor can
    // actually be ruled out by them. A single hint-less extractor
    // matches every message, so the scan would have to read each body
    // anyway and the extra header read would be pure overhead.
    if !config.extractors.is_empty() && config.extractors.iter().all(Extractor::constrains_headers)
    {
        let before = all.len();
        // rayon's indexed parallel iterators collect in input order,
        // so the sorted enumeration (and hence `--limit`) survives.
        all = all
            .par_iter()
            .filter(|p| could_match_any(p, config.extractors))
            .cloned()
            .collect();
        info!(
            scanned = before,
            matched = all.len(),
            skipped = before - all.len(),
            "header prefilter narrowed the scan"
        );
    }

    let take = config.limit.map(|n| all.len().min(n)).unwrap_or(all.len());
    let messages = &all[..take];
    info!(count = messages.len(), "processing messages");

    let pb = make_progress_bar(messages.len() as u64);
    messages.par_iter().for_each(|path| {
        pb.set_message(format!("{}", path.display()));
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "read failed");
                pb.inc(1);
                return;
            }
        };
        let source = format!("maildir {}", path.display());
        let result = pipeline::run(
            &raw,
            &source,
            config.extractors,
            config.targets,
            pipeline::DkimPolicy::Enforce,
            config.dry_run,
            None,
        );
        if let Err(e) = result {
            warn!(path = %path.display(), error = %e, "pipeline failed");
        }
        pb.inc(1);
    });
    pb.finish_and_clear();
    Ok(())
}

fn make_progress_bar(len: u64) -> ProgressBar {
    let pb = ProgressBar::new(len).with_style(
        ProgressStyle::with_template("{spinner} [{elapsed_precise}] [{bar:40}] {pos}/{len} {msg}")
            .expect("static template is valid")
            .progress_chars("=> "),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(200));
    pb
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_maildir(root: &Path) {
        fs::create_dir_all(root.join("cur")).unwrap();
        fs::create_dir_all(root.join("new")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();
    }

    #[test]
    fn discover_flat_maildir() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path());
        let folders = discover_folders(td.path(), false).unwrap();
        assert_eq!(folders, vec![td.path().to_path_buf()]);
    }

    #[test]
    fn discover_recursive_picks_up_dotdirs() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path());
        make_maildir(&td.path().join(".archive"));
        make_maildir(&td.path().join(".lists"));
        // A non-Maildir dotdir should be skipped.
        fs::create_dir_all(td.path().join(".notes")).unwrap();
        let folders = discover_folders(td.path(), true).unwrap();
        assert_eq!(
            folders,
            vec![
                td.path().to_path_buf(),
                td.path().join(".archive"),
                td.path().join(".lists"),
            ]
        );
    }

    #[test]
    fn discover_flat_ignores_dotdirs() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path());
        make_maildir(&td.path().join(".archive"));
        let folders = discover_folders(td.path(), false).unwrap();
        assert_eq!(folders, vec![td.path().to_path_buf()]);
    }

    #[test]
    fn discover_rejects_non_maildir() {
        let td = tempfile::tempdir().unwrap();
        let err = discover_folders(td.path(), false).unwrap_err();
        assert!(
            err.to_string().contains("does not look like a Maildir"),
            "{err}"
        );
    }

    #[test]
    fn header_end_found_for_lf_endings() {
        assert_eq!(find_header_end(b"From: a\nSubject: b\n\nbody"), Some(20));
    }

    #[test]
    fn header_end_found_for_crlf_endings() {
        assert_eq!(
            find_header_end(b"From: a\r\nSubject: b\r\n\r\nbody"),
            Some(23)
        );
    }

    #[test]
    fn header_end_prefers_the_earlier_terminator() {
        // A bare LF blank line before the CRLF one ends the block first.
        assert_eq!(find_header_end(b"From: a\n\nx\r\n\r\ny"), Some(9));
    }

    #[test]
    fn header_end_absent_without_blank_line() {
        assert_eq!(find_header_end(b"From: a\nSubject: b\n"), None);
    }

    #[test]
    fn read_header_block_returns_short_file_whole() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("msg");
        fs::write(&path, b"From: a@example.com\n\nbody").unwrap();
        assert_eq!(
            read_header_block(&path).unwrap(),
            Some(b"From: a@example.com\n\nbody".to_vec())
        );
    }

    #[test]
    fn read_header_block_gives_up_when_headers_exceed_the_window() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("msg");
        // A header block longer than the peek window, so the blank
        // line separating body from headers is never in view.
        let mut raw = b"From: a@example.com\n".to_vec();
        while raw.len() <= HEADER_PEEK_BYTES {
            raw.extend_from_slice(b"X-Pad: 0123456789abcdef\n");
        }
        raw.extend_from_slice(b"\nbody");
        fs::write(&path, &raw).unwrap();
        assert_eq!(read_header_block(&path).unwrap(), None);
    }

    fn fixture_extractors() -> Vec<Extractor> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extractors");
        crate::extractor::discover(&[dir]).unwrap()
    }

    fn named(name: &str) -> Vec<Extractor> {
        fixture_extractors()
            .into_iter()
            .filter(|ex| ex.name == name)
            .collect()
    }

    #[test]
    fn could_match_any_keeps_a_message_from_a_declared_domain() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("msg");
        fs::write(
            &path,
            b"From: Airline <no-reply@flight.fixture.test>\nSubject: fixture-flight AB123\n\nbody",
        )
        .unwrap();
        assert!(could_match_any(&path, &named("fixture-flight")));
    }

    #[test]
    fn could_match_any_drops_a_message_from_another_domain() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("msg");
        fs::write(
            &path,
            b"From: Someone <hi@unrelated.example>\nSubject: fixture-flight AB123\n\nbody",
        )
        .unwrap();
        assert!(!could_match_any(&path, &named("fixture-flight")));
    }

    #[test]
    fn could_match_any_drops_a_matching_domain_with_a_non_matching_subject() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("msg");
        fs::write(
            &path,
            b"From: Airline <no-reply@flight.fixture.test>\nSubject: newsletter\n\nbody",
        )
        .unwrap();
        assert!(!could_match_any(&path, &named("fixture-flight")));
    }

    #[test]
    fn could_match_any_keeps_an_unreadable_file() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("does-not-exist");
        assert!(could_match_any(&path, &named("fixture-flight")));
    }

    #[test]
    fn prefilter_preserves_input_order() {
        // `--limit` is only reproducible if filtering keeps the sorted
        // enumeration order, which relies on rayon's indexed collect.
        let td = tempfile::tempdir().unwrap();
        let mut expected = Vec::new();
        let mut paths = Vec::new();
        for i in 0..200 {
            let path = td.path().join(format!("{i:04}"));
            let matching = i % 3 == 0;
            let from = if matching {
                "no-reply@flight.fixture.test"
            } else {
                "hi@unrelated.example"
            };
            fs::write(
                &path,
                format!("From: <{from}>\nSubject: fixture-flight AB123\n\nbody"),
            )
            .unwrap();
            if matching {
                expected.push(path.clone());
            }
            paths.push(path);
        }
        let extractors = named("fixture-flight");
        let kept: Vec<PathBuf> = paths
            .par_iter()
            .filter(|p| could_match_any(p, &extractors))
            .cloned()
            .collect();
        assert_eq!(kept, expected);
    }

    #[test]
    fn constrains_headers_reflects_the_manifest_hints() {
        assert!(named("fixture-flight")[0].constrains_headers());
        // `fixture-ics-pass` declares only `requires:`, no header hints.
        assert!(!named("fixture-ics-pass")[0].constrains_headers());
    }

    #[test]
    fn collect_reads_cur_and_new_but_not_tmp() {
        let td = tempfile::tempdir().unwrap();
        make_maildir(td.path());
        fs::write(td.path().join("cur/1"), b"a").unwrap();
        fs::write(td.path().join("new/2"), b"b").unwrap();
        fs::write(td.path().join("tmp/3"), b"c").unwrap();
        let mut msgs = collect_messages(td.path()).unwrap();
        msgs.sort();
        assert_eq!(msgs, vec![td.path().join("cur/1"), td.path().join("new/2")]);
    }
}
