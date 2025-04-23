//! Target for `ticket` artifacts.
//!
//! Tickets are opaque binary blobs: boarding passes (`.pdf`,
//! `.pkpass`), QR codes (image formats), etc. We can't peek into them
//! for a meaningful date, so the caller supplies the year via
//! [`TicketSink::file_ticket`]'s `year` argument. The pipeline picks
//! that year from a sibling event/reservation artifact emitted in the
//! same extractor run (a flight ticket lives in the same run as the
//! flight's VEVENT); failing that, the message's `Date:` header;
//! failing that, the current year.
//!
//! Two sink variants:
//! - [`TicketSink::LocalDir`]: files at `<dir>/<year>/<slug>.<ext>`.
//! - [`TicketSink::Webdav`]: PUTs to `<base_url>/<year>/<slug>.<ext>`.
//!
//! Same `<slug>` + `<ext>` overwrites in place either way, matching
//! the PUT-by-name idempotency used elsewhere.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::info;

use super::FileOutcome;
use super::sink::{sanitize_ext, slugify, write_atomic};
use super::webdav::{PutOutcome, WebdavSink};

/// Where to file `ticket` artifacts.
pub enum TicketSink {
    LocalDir(PathBuf),
    Webdav(WebdavSink),
}

impl TicketSink {
    /// File `src` (a binary blob on disk) under the kind/year/slug/ext
    /// scheme the sink defines.
    pub fn file_ticket(&self, src: &Path, slug: &str, ext: &str, year: i32) -> Result<FileOutcome> {
        let slug = slugify(slug, false);
        if slug.is_empty() {
            bail!("{}: empty slug after sanitisation", src.display());
        }
        let ext = sanitize_ext(ext)?;

        match self {
            TicketSink::LocalDir(dir) => file_to_dir(src, &slug, &ext, year, dir),
            TicketSink::Webdav(sink) => file_to_webdav(src, &slug, &ext, year, sink),
        }
    }
}

fn file_to_dir(src: &Path, slug: &str, ext: &str, year: i32, dir: &Path) -> Result<FileOutcome> {
    let target = dir.join(format!("{year:04}")).join(format!("{slug}.{ext}"));

    let body = fs::read(src).with_context(|| format!("reading ticket source {}", src.display()))?;

    let existed = target.exists();
    write_atomic(&target, &body)?;

    if existed {
        info!(target = %target.display(), "ticket updated");
        Ok(FileOutcome::Updated(target.display().to_string()))
    } else {
        info!(target = %target.display(), "ticket created");
        Ok(FileOutcome::Created(target.display().to_string()))
    }
}

fn file_to_webdav(
    src: &Path,
    slug: &str,
    ext: &str,
    year: i32,
    sink: &WebdavSink,
) -> Result<FileOutcome> {
    let body = fs::read(src).with_context(|| format!("reading ticket source {}", src.display()))?;
    let sub_path = format!("{year:04}/{slug}.{ext}");
    // Best-guess content-type from extension. Servers typically don't
    // care for opaque uploads, but a sensible value is nicer than
    // application/octet-stream for the common cases.
    let content_type = match ext {
        "pdf" => "application/pdf",
        "pkpass" => "application/vnd.apple.pkpass",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    };
    let outcome = sink.put(&sub_path, content_type, body)?;
    Ok(match outcome {
        PutOutcome::Created(url) => FileOutcome::Created(url),
        PutOutcome::Updated(url) => FileOutcome::Updated(url),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_dir_files_under_year() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("boarding-pass.pdf");
        std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

        let sink = TicketSink::LocalDir(tmp.path().to_path_buf());
        let outcome = sink
            .file_ticket(&src, "EasyJet-EZY2521", "PDF", 2024)
            .unwrap();
        let path = match outcome {
            FileOutcome::Created(p) => p,
            FileOutcome::Updated(_) => panic!("expected Created on first write"),
        };
        let expected = tmp.path().join("2024/easyjet-ezy2521.pdf");
        assert_eq!(PathBuf::from(&path), expected);
        assert!(expected.exists());
    }

    #[test]
    fn local_dir_overwrites_on_second_write() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("boarding-pass.pdf");
        std::fs::write(&src, b"v1").unwrap();

        let sink = TicketSink::LocalDir(tmp.path().to_path_buf());
        let _ = sink.file_ticket(&src, "flight", "pdf", 2024).unwrap();

        std::fs::write(&src, b"v2").unwrap();
        let outcome = sink.file_ticket(&src, "flight", "pdf", 2024).unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));

        let target = tmp.path().join("2024/flight.pdf");
        assert_eq!(std::fs::read(&target).unwrap(), b"v2");
    }
}
