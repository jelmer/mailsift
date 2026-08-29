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
//!
//! Alongside each blob goes a `<slug>.json` sidecar. Nothing can be
//! read out of a PDF or a pkpass, so the sidecar records what we knew
//! at filing time: which file the blob landed in, its content type,
//! and the identifying fields ([`TicketMeta`]) lifted from the sibling
//! reservation in the same extractor run. That makes the ticket
//! directory searchable without opening every attachment.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tracing::info;

use super::FileOutcome;
use super::sink::{sanitize_ext, slugify, write_atomic};
use super::webdav::{PutOutcome, WebdavSink};

/// Identifying fields copied from the sibling reservation artifact, if
/// there was one. All optional: a ticket can arrive with no
/// reservation beside it, in which case the sidecar carries only the
/// blob's own details.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reservation_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub under_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Body of the `<slug>.json` sidecar written beside each ticket blob.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Sidecar<'a> {
    slug: &'a str,
    /// Basename of the blob this sidecar describes.
    file: String,
    content_type: &'a str,
    #[serde(flatten)]
    meta: &'a TicketMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    received_at: Option<String>,
}

/// Best-guess content-type from the extension. Servers typically
/// don't care for opaque uploads, but a sensible value is nicer than
/// application/octet-stream for the common cases.
fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "pkpass" => "application/vnd.apple.pkpass",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

/// Render the sidecar body for a ticket blob.
fn sidecar_body(
    slug: &str,
    ext: &str,
    meta: &TicketMeta,
    received_at_epoch: Option<i64>,
) -> Result<String> {
    let sidecar = Sidecar {
        slug,
        file: format!("{slug}.{ext}"),
        content_type: content_type_for(ext),
        meta,
        received_at: received_at_epoch.and_then(super::json_target::format_received_at),
    };
    serde_json::to_string_pretty(&sidecar).context("serialising ticket sidecar")
}

/// Where to file `ticket` artifacts.
pub enum TicketSink {
    LocalDir(PathBuf),
    Webdav(WebdavSink),
}

impl TicketSink {
    /// File `src` (a binary blob on disk) under the kind/year/slug/ext
    /// scheme the sink defines, plus a `<slug>.json` sidecar
    /// describing it.
    ///
    /// The returned [`FileOutcome`] describes the blob; a sidecar
    /// failure is reported through the `Err` path, since a ticket
    /// whose metadata silently went missing is worse than a loud one.
    pub fn file_ticket(
        &self,
        src: &Path,
        slug: &str,
        ext: &str,
        year: i32,
        meta: &TicketMeta,
        received_at_epoch: Option<i64>,
    ) -> Result<FileOutcome> {
        let slug = slugify(slug, false);
        if slug.is_empty() {
            bail!("{}: empty slug after sanitisation", src.display());
        }
        let ext = sanitize_ext(ext)?;
        let sidecar = sidecar_body(&slug, &ext, meta, received_at_epoch)?;

        match self {
            TicketSink::LocalDir(dir) => file_to_dir(src, &slug, &ext, year, dir, &sidecar),
            TicketSink::Webdav(sink) => file_to_webdav(src, &slug, &ext, year, sink, &sidecar),
        }
    }
}

fn file_to_dir(
    src: &Path,
    slug: &str,
    ext: &str,
    year: i32,
    dir: &Path,
    sidecar: &str,
) -> Result<FileOutcome> {
    let year_dir = dir.join(format!("{year:04}"));
    let target = year_dir.join(format!("{slug}.{ext}"));

    let body = fs::read(src).with_context(|| format!("reading ticket source {}", src.display()))?;

    let existed = target.exists();
    write_atomic(&target, &body)?;
    write_atomic(&year_dir.join(format!("{slug}.json")), sidecar.as_bytes())?;

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
    sidecar: &str,
) -> Result<FileOutcome> {
    let body = fs::read(src).with_context(|| format!("reading ticket source {}", src.display()))?;
    let sub_path = format!("{year:04}/{slug}.{ext}");
    let outcome = sink.put(&sub_path, content_type_for(ext), body)?;
    sink.put(
        &format!("{year:04}/{slug}.json"),
        "application/json",
        sidecar.as_bytes().to_vec(),
    )?;
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
            .file_ticket(
                &src,
                "EasyJet-EZY2521",
                "PDF",
                2024,
                &TicketMeta::default(),
                None,
            )
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
        let _ = sink
            .file_ticket(&src, "flight", "pdf", 2024, &TicketMeta::default(), None)
            .unwrap();

        std::fs::write(&src, b"v2").unwrap();
        let outcome = sink
            .file_ticket(&src, "flight", "pdf", 2024, &TicketMeta::default(), None)
            .unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));

        let target = tmp.path().join("2024/flight.pdf");
        assert_eq!(std::fs::read(&target).unwrap(), b"v2");
    }

    #[test]
    fn sidecar_lands_beside_the_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("boarding-pass.pdf");
        std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

        let meta = TicketMeta {
            reservation_number: Some("ABC123".into()),
            under_name: Some("J Vernooij".into()),
            provider: Some("easyJet".into()),
        };
        let sink = TicketSink::LocalDir(tmp.path().to_path_buf());
        // 2026-08-27T09:30:00Z
        sink.file_ticket(
            &src,
            "easyjet-ezy2521",
            "pdf",
            2026,
            &meta,
            Some(1787823000),
        )
        .unwrap();

        let body = std::fs::read_to_string(tmp.path().join("2026/easyjet-ezy2521.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["slug"], "easyjet-ezy2521");
        assert_eq!(v["file"], "easyjet-ezy2521.pdf");
        assert_eq!(v["contentType"], "application/pdf");
        assert_eq!(v["reservationNumber"], "ABC123");
        assert_eq!(v["underName"], "J Vernooij");
        assert_eq!(v["provider"], "easyJet");
        assert_eq!(v["receivedAt"], "2026-08-27T09:30:00Z");
    }

    #[test]
    fn sidecar_omits_absent_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("qr.png").to_path_buf();
        std::fs::write(&src, b"\x89PNG").unwrap();

        let sink = TicketSink::LocalDir(tmp.path().to_path_buf());
        sink.file_ticket(&src, "gig", "png", 2026, &TicketMeta::default(), None)
            .unwrap();

        let body = std::fs::read_to_string(tmp.path().join("2026/gig.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["contentType"], "image/png");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("reservationNumber"));
        assert!(!obj.contains_key("receivedAt"));
    }

    #[test]
    fn sidecar_uses_the_sanitised_slug_and_ext() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("t.pdf");
        std::fs::write(&src, b"x").unwrap();

        let sink = TicketSink::LocalDir(tmp.path().to_path_buf());
        sink.file_ticket(
            &src,
            "NS // Reizigers!!",
            "PDF",
            2026,
            &TicketMeta::default(),
            None,
        )
        .unwrap();

        let body = std::fs::read_to_string(tmp.path().join("2026/ns-reizigers.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["slug"], "ns-reizigers");
        assert_eq!(v["file"], "ns-reizigers.pdf");
    }
}
