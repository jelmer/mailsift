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

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::pipeline::{self, PipelineTargets};

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
