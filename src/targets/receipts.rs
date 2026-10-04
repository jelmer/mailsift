//! Target for `receipt` artifacts.
//!
//! Each `.receipt.json` artifact is parsed for its `merchant` and
//! `orderNumber` (loosely schema.org `Order`-shaped). Three sink
//! variants:
//!
//! - [`ReceiptSink::LocalDir`]: files at
//!   `<dir>/<year>/<merchant_slug>-<orderNumber>.json`.
//! - [`ReceiptSink::Webdav`]: PUTs to
//!   `<base_url>/<year>/<merchant_slug>-<orderNumber>.json`.
//! - [`ReceiptSink::Forward`]: emails the original RFC822 message as a
//!   `message/rfc822` attachment to a configured recipient.
//!
//! Year derivation falls through `orderDate`, then `date`; failing
//! both, the current year. Slug rules match the bills/tickets targets
//! (lowercase ASCII alphanumerics plus `_`, `.`, `+`).
//!
//! Extractors may also emit companion blobs (`<slug>.receipt.pdf` etc.).
//! Those go through [`ReceiptSink::file_receipt_blob`], which requires
//! a same-slug `.receipt.json` sibling in the same run and files the
//! pair under a common `<merchant>-<order>` name. Blobs without a
//! sibling are dropped with a warning by the pipeline: the extractor
//! should have emitted the structured record too. The
//! [`ReceiptSink::Forward`] variant is a no-op for companion blobs:
//! the original RFC822 was already forwarded on the JSON side.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::FileOutcome;
use super::json_target::{self, derive_year, first_non_empty, read_and_parse};
use super::mail_forward::{self, MailForwarder};
use super::sink::{Merge, sanitize_ext, slugify, update_file};
use super::tickets::content_type_for;
use super::webdav::WebdavSink;

/// Where to file `receipt` artifacts.
pub enum ReceiptSink {
    LocalDir(PathBuf),
    Webdav(WebdavSink),
    Forward(MailForwarder),
}

/// Shape we read out of a `.receipt.json` artifact. Loosely schema.org
/// `Order`-shaped; unknown fields are ignored so extractors can emit
/// richer JSON without breaking the target.
#[derive(Debug, Deserialize)]
struct Receipt {
    merchant: Option<String>,
    seller: Option<String>,
    #[serde(rename = "orderNumber")]
    order_number: Option<String>,
    identifier: Option<String>,
    #[serde(rename = "orderDate")]
    order_date: Option<String>,
    date: Option<String>,
}

impl Receipt {
    fn merchant(&self) -> Option<&str> {
        first_non_empty([self.merchant.as_deref(), self.seller.as_deref()])
    }

    fn order(&self) -> Option<&str> {
        first_non_empty([self.order_number.as_deref(), self.identifier.as_deref()])
    }

    fn date_candidates(&self) -> [Option<&str>; 2] {
        [self.order_date.as_deref(), self.date.as_deref()]
    }
}

impl ReceiptSink {
    /// File the receipt to whichever sink this is.
    ///
    /// `raw_message` is the original RFC822 that produced the receipt.
    /// Only the [`ReceiptSink::Forward`] variant uses it; the
    /// LocalDir and WebDAV variants ignore it. Threading it through
    /// the API uniformly keeps the pipeline call site simple.
    pub fn file_receipt(
        &self,
        src: &Path,
        raw_message: &[u8],
        received_at_epoch: Option<i64>,
    ) -> Result<FileOutcome> {
        let (body, receipt) = read_and_parse::<Receipt>(src, "receipt")?;

        let merchant = receipt
            .merchant()
            .ok_or_else(|| anyhow!("{}: missing 'merchant'", src.display()))?;
        let order = receipt
            .order()
            .ok_or_else(|| anyhow!("{}: missing 'orderNumber'", src.display()))?;
        let year = derive_year(receipt.date_candidates());

        let merchant_slug = slugify(merchant, false);
        let order_slug = slugify(order, false);
        if merchant_slug.is_empty() || order_slug.is_empty() {
            bail!(
                "{}: empty slug after sanitisation (merchant={merchant:?} order={order:?})",
                src.display()
            );
        }

        if let ReceiptSink::Forward(fwd) = self {
            let hint = mail_forward::subject_hint(raw_message);
            fwd.forward(raw_message, &hint)?;
            return Ok(FileOutcome::Created(format!("forwarded ({hint})")));
        }
        let body_out = json_target::body_with_received_at(&body, received_at_epoch);
        let filename = format!("{merchant_slug}-{order_slug}.json");
        let received_at = json_target::received_at_in(body_out.as_bytes())?;
        self.update(
            year,
            &filename,
            "application/json",
            "receipt",
            &json_target::replace_unless_newer(&filename, body_out.as_bytes(), received_at),
        )
    }

    /// File a companion blob (typically a PDF) alongside a receipt.
    ///
    /// `pair` is the sanitised `(merchant_slug, order_slug, year)`
    /// pulled from a same-slug `.receipt.json` sibling in the same
    /// extractor run; the blob is filed under
    /// `<year>/<merchant>-<order>.<ext>` so it sits beside the JSON.
    ///
    /// `received_at` is the date that sibling was filed under. A blob
    /// has no date of its own, so it follows the JSON: when the record
    /// on file came from a newer message, the blob on file did too and
    /// is kept.
    ///
    /// The [`ReceiptSink::Forward`] variant is a no-op here: the
    /// original RFC822 was already forwarded on the JSON receipt.
    pub fn file_receipt_blob(
        &self,
        src: &Path,
        ext: &str,
        pair: (&str, &str, i32),
        received_at: Option<DateTime<Utc>>,
    ) -> Result<Option<FileOutcome>> {
        // The forward variant has nothing to do for companion blobs; the
        // original RFC822 was already forwarded on the JSON side. Short-
        // circuit before reading the file so we don't slurp its bytes
        // just to throw them away.
        if matches!(self, ReceiptSink::Forward(_)) {
            return Ok(None);
        }
        let ext = sanitize_ext(ext)?;
        let (merchant, order, year) = pair;
        let name_stem = format!("{merchant}-{order}");
        let filename = format!("{name_stem}.{ext}");
        let record = format!("{name_stem}.json");
        let body =
            fs::read(src).with_context(|| format!("reading receipt blob {}", src.display()))?;
        self.update(
            year,
            &filename,
            content_type_for(&ext),
            "receipt blob",
            &|_| Ok((!self.filed_from_newer(year, &record, received_at)?).then(|| body.clone())),
        )
        .map(Some)
    }

    /// Whether the receipt JSON at `<year>/<json_filename>`, if any,
    /// was filed from a newer message than one dated `incoming`. Not
    /// callable on the forward variant, which keeps no records.
    fn filed_from_newer(
        &self,
        year: i32,
        json_filename: &str,
        incoming: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        match self {
            ReceiptSink::LocalDir(dir) => json_target::filed_from_newer(
                &dir.join(format!("{year:04}")).join(json_filename),
                incoming,
            ),
            ReceiptSink::Webdav(sink) => {
                sink.filed_from_newer(&format!("{year:04}/{json_filename}"), incoming)
            }
            ReceiptSink::Forward(_) => {
                unreachable!("callers short-circuit the forward variant before reaching here")
            }
        }
    }

    /// Replace `<year>/<filename>`, at whichever local or WebDAV backend
    /// this sink wraps, with whatever `merge` makes of what is there.
    /// Both blob and JSON call sites route through here so a new
    /// backend only has to be added once. Not callable on the forward
    /// variant; callers must short-circuit it first.
    fn update(
        &self,
        year: i32,
        filename: &str,
        content_type: &str,
        log_kind: &str,
        merge: &Merge<'_>,
    ) -> Result<FileOutcome> {
        match self {
            ReceiptSink::LocalDir(dir) => update_file(
                &dir.join(format!("{year:04}")).join(filename),
                log_kind,
                merge,
            ),
            ReceiptSink::Webdav(sink) => sink.update(
                &format!("{year:04}/{filename}"),
                content_type,
                log_kind,
                merge,
            ),
            ReceiptSink::Forward(_) => {
                unreachable!("callers short-circuit the forward variant before reaching here")
            }
        }
    }
}

/// Parse `body` as a receipt JSON and return the paired
/// `(merchant_slug, order_slug, year)` that a companion blob should be
/// filed under, or `None` if a required field is missing.
///
/// Used by the pipeline to resolve a sibling `.receipt.json` to the
/// name a same-slug `.receipt.<ext>` blob should be filed under.
pub fn paired_name_from_json(body: &str) -> Option<(String, String, i32)> {
    let receipt: Receipt = serde_json::from_str(body).ok()?;
    let merchant_slug = slugify(receipt.merchant()?, false);
    let order_slug = slugify(receipt.order()?, false);
    if merchant_slug.is_empty() || order_slug.is_empty() {
        return None;
    }
    let year = derive_year(receipt.date_candidates());
    Some((merchant_slug, order_slug, year))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_from_order_date() {
        let r: Receipt = serde_json::from_value(serde_json::json!({"orderDate": "2024-12-05"}))
            .expect("valid receipt");
        assert_eq!(derive_year(r.date_candidates()), 2024);
    }

    #[test]
    fn merchant_falls_back_to_seller() {
        let r: Receipt = serde_json::from_value(serde_json::json!({
            "seller": "Cafe Sample"
        }))
        .unwrap();
        assert_eq!(r.merchant(), Some("Cafe Sample"));
    }

    #[test]
    fn local_dir_files_under_year() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("receipt.json");
        std::fs::write(
            &src,
            br#"{"merchant": "Amazon", "orderNumber": "ABC123", "orderDate": "2024-08-10"}"#,
        )
        .unwrap();

        let sink = ReceiptSink::LocalDir(tmp.path().to_path_buf());
        let outcome = sink.file_receipt(&src, b"", None).unwrap();
        let path = match outcome {
            FileOutcome::Created(p) => p,
            other => panic!("expected Created on first write, got {other:?}"),
        };
        let expected = tmp.path().join("2024/amazon-abc123.json");
        assert_eq!(PathBuf::from(&path), expected);
        assert!(expected.exists());
    }

    #[test]
    fn paired_name_from_json_derives_slugs_and_year() {
        let body =
            r#"{"merchant":"Digital Ocean","orderNumber":"INV-42","orderDate":"2026-08-01"}"#;
        let (merchant, order, year) = paired_name_from_json(body).unwrap();
        assert_eq!(merchant, "digital-ocean");
        assert_eq!(order, "inv-42");
        assert_eq!(year, 2026);
    }

    #[test]
    fn paired_name_missing_fields_returns_none() {
        let body = r#"{"orderNumber":"only-order"}"#;
        assert!(paired_name_from_json(body).is_none());
    }

    #[test]
    fn blob_paired_lands_beside_json() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("blob.pdf");
        std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

        let sink = ReceiptSink::LocalDir(tmp.path().to_path_buf());
        let outcome = sink
            .file_receipt_blob(&src, "pdf", ("digital-ocean", "inv-42", 2026), None)
            .unwrap()
            .unwrap();
        let path = match outcome {
            FileOutcome::Created(p) => p,
            other => panic!("expected Created, got {other:?}"),
        };
        assert_eq!(
            PathBuf::from(&path),
            tmp.path().join("2026/digital-ocean-inv-42.pdf")
        );
    }

    #[test]
    fn webdav_older_mail_does_not_replace_receipt_or_its_blob() {
        let server = crate::targets::fake_dav::FakeDav::start();
        let sink = ReceiptSink::Webdav(server.webdav_sink());

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("receipt.json");
        let blob = tmp.path().join("receipt.pdf");
        let pair = ("digital-ocean", "inv-42", 2026);
        let at = |epoch| DateTime::from_timestamp(epoch, 0);

        write_receipt(&src, 12.0);
        std::fs::write(&blob, b"correction").unwrap();
        sink.file_receipt(&src, b"", Some(CORRECTION_SENT)).unwrap();
        sink.file_receipt_blob(&blob, "pdf", pair, at(CORRECTION_SENT))
            .unwrap();

        write_receipt(&src, 10.0);
        std::fs::write(&blob, b"invoice").unwrap();
        let outcome = sink.file_receipt(&src, b"", Some(INVOICE_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Kept(_)));
        let outcome = sink
            .file_receipt_blob(&blob, "pdf", pair, at(INVOICE_SENT))
            .unwrap();
        assert!(matches!(outcome, Some(FileOutcome::Kept(_))));

        let json = server.resource("/2026/digital-ocean-inv-42.json").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["price"], 12.0);
        assert_eq!(
            server.resource("/2026/digital-ocean-inv-42.pdf").unwrap(),
            b"correction"
        );
    }

    #[test]
    fn webdav_newer_mail_replaces_receipt_and_its_blob() {
        let server = crate::targets::fake_dav::FakeDav::start();
        let sink = ReceiptSink::Webdav(server.webdav_sink());

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("receipt.json");
        let blob = tmp.path().join("receipt.pdf");
        let pair = ("digital-ocean", "inv-42", 2026);
        let at = |epoch| DateTime::from_timestamp(epoch, 0);

        write_receipt(&src, 10.0);
        std::fs::write(&blob, b"invoice").unwrap();
        sink.file_receipt(&src, b"", Some(INVOICE_SENT)).unwrap();
        sink.file_receipt_blob(&blob, "pdf", pair, at(INVOICE_SENT))
            .unwrap();

        write_receipt(&src, 12.0);
        std::fs::write(&blob, b"correction").unwrap();
        let outcome = sink.file_receipt(&src, b"", Some(CORRECTION_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        let outcome = sink
            .file_receipt_blob(&blob, "pdf", pair, at(CORRECTION_SENT))
            .unwrap();
        assert!(matches!(outcome, Some(FileOutcome::Updated(_))));

        let json = server.resource("/2026/digital-ocean-inv-42.json").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["price"], 12.0);
        assert_eq!(
            server.resource("/2026/digital-ocean-inv-42.pdf").unwrap(),
            b"correction"
        );
    }

    // 2026-08-01T00:00:00Z and a corrected invoice sent a week later.
    const INVOICE_SENT: i64 = 1785542400;
    const CORRECTION_SENT: i64 = 1786147200;

    fn write_receipt(path: &Path, price: f64) {
        let body = serde_json::json!({
            "merchant": "Digital Ocean", "orderNumber": "INV-42",
            "orderDate": "2026-08-01", "price": price,
        });
        std::fs::write(path, body.to_string()).unwrap();
    }

    fn filed_price(dir: &Path) -> serde_json::Value {
        let body = std::fs::read(dir.join("2026/digital-ocean-inv-42.json")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        v["price"].clone()
    }

    #[test]
    fn older_mail_does_not_replace_receipt_or_its_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("receipt.json");
        let blob = tmp.path().join("receipt.pdf");
        let dir = tmp.path().join("out");
        let sink = ReceiptSink::LocalDir(dir.clone());
        let pair = ("digital-ocean", "inv-42", 2026);
        let at = |epoch| DateTime::from_timestamp(epoch, 0);

        write_receipt(&src, 12.0);
        std::fs::write(&blob, b"correction").unwrap();
        sink.file_receipt(&src, b"", Some(CORRECTION_SENT)).unwrap();
        sink.file_receipt_blob(&blob, "pdf", pair, at(CORRECTION_SENT))
            .unwrap();

        write_receipt(&src, 10.0);
        std::fs::write(&blob, b"invoice").unwrap();
        let outcome = sink.file_receipt(&src, b"", Some(INVOICE_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Kept(_)));
        let outcome = sink
            .file_receipt_blob(&blob, "pdf", pair, at(INVOICE_SENT))
            .unwrap();
        assert!(matches!(outcome, Some(FileOutcome::Kept(_))));

        assert_eq!(filed_price(&dir), 12.0);
        assert_eq!(
            std::fs::read(dir.join("2026/digital-ocean-inv-42.pdf")).unwrap(),
            b"correction"
        );
    }

    #[test]
    fn newer_mail_replaces_receipt_and_its_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("receipt.json");
        let blob = tmp.path().join("receipt.pdf");
        let dir = tmp.path().join("out");
        let sink = ReceiptSink::LocalDir(dir.clone());
        let pair = ("digital-ocean", "inv-42", 2026);
        let at = |epoch| DateTime::from_timestamp(epoch, 0);

        write_receipt(&src, 10.0);
        std::fs::write(&blob, b"invoice").unwrap();
        sink.file_receipt(&src, b"", Some(INVOICE_SENT)).unwrap();
        sink.file_receipt_blob(&blob, "pdf", pair, at(INVOICE_SENT))
            .unwrap();

        write_receipt(&src, 12.0);
        std::fs::write(&blob, b"correction").unwrap();
        let outcome = sink.file_receipt(&src, b"", Some(CORRECTION_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        let outcome = sink
            .file_receipt_blob(&blob, "pdf", pair, at(CORRECTION_SENT))
            .unwrap();
        assert!(matches!(outcome, Some(FileOutcome::Updated(_))));

        assert_eq!(filed_price(&dir), 12.0);
        assert_eq!(
            std::fs::read(dir.join("2026/digital-ocean-inv-42.pdf")).unwrap(),
            b"correction"
        );
    }
}
