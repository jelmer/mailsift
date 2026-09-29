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
use serde::Deserialize;
use tracing::info;

use super::FileOutcome;
use super::json_target::{derive_year, first_non_empty};
use super::mail_forward::{self, MailForwarder};
use super::sink::{sanitize_ext, slugify, write_atomic};
use super::tickets::content_type_for;
use super::webdav::{PutOutcome, WebdavSink};

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
        let body = fs::read_to_string(src)
            .with_context(|| format!("reading receipt source {}", src.display()))?;
        let receipt: Receipt = serde_json::from_str(&body)
            .with_context(|| format!("parsing receipt JSON {}", src.display()))?;

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

        match self {
            ReceiptSink::LocalDir(dir) => {
                let body_out = super::json_target::body_with_received_at(&body, received_at_epoch);
                file_to_dir(&merchant_slug, &order_slug, year, body_out.as_bytes(), dir)
            }
            ReceiptSink::Webdav(sink) => {
                let body_out = super::json_target::body_with_received_at(&body, received_at_epoch);
                file_to_webdav(
                    &merchant_slug,
                    &order_slug,
                    year,
                    body_out.into_bytes(),
                    sink,
                )
            }
            ReceiptSink::Forward(fwd) => {
                let hint = mail_forward::subject_hint(raw_message);
                fwd.forward(raw_message, &hint)?;
                Ok(FileOutcome::Created(format!("forwarded ({hint})")))
            }
        }
    }

    /// File a companion blob (typically a PDF) alongside a receipt.
    ///
    /// `pair` is the sanitised `(merchant_slug, order_slug, year)`
    /// pulled from a same-slug `.receipt.json` sibling in the same
    /// extractor run; the blob is filed under
    /// `<year>/<merchant>-<order>.<ext>` so it sits beside the JSON.
    ///
    /// The [`ReceiptSink::Forward`] variant is a no-op here: the
    /// original RFC822 was already forwarded on the JSON receipt.
    pub fn file_receipt_blob(
        &self,
        src: &Path,
        ext: &str,
        pair: (&str, &str, i32),
    ) -> Result<Option<FileOutcome>> {
        let ext = sanitize_ext(ext)?;
        let (merchant, order, year) = pair;
        let name_stem = format!("{merchant}-{order}");
        match self {
            ReceiptSink::LocalDir(dir) => {
                let body = fs::read(src)
                    .with_context(|| format!("reading receipt blob {}", src.display()))?;
                Ok(Some(file_blob_to_dir(&name_stem, &ext, year, &body, dir)?))
            }
            ReceiptSink::Webdav(sink) => {
                let body = fs::read(src)
                    .with_context(|| format!("reading receipt blob {}", src.display()))?;
                Ok(Some(file_blob_to_webdav(
                    &name_stem, &ext, year, body, sink,
                )?))
            }
            ReceiptSink::Forward(_) => Ok(None),
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

fn file_to_dir(
    merchant_slug: &str,
    order_slug: &str,
    year: i32,
    body: &[u8],
    dir: &Path,
) -> Result<FileOutcome> {
    let target = dir
        .join(format!("{year:04}"))
        .join(format!("{merchant_slug}-{order_slug}.json"));

    let existed = target.exists();
    write_atomic(&target, body)?;

    if existed {
        info!(target = %target.display(), "receipt updated");
        Ok(FileOutcome::Updated(target.display().to_string()))
    } else {
        info!(target = %target.display(), "receipt created");
        Ok(FileOutcome::Created(target.display().to_string()))
    }
}

fn file_to_webdav(
    merchant_slug: &str,
    order_slug: &str,
    year: i32,
    body: Vec<u8>,
    sink: &WebdavSink,
) -> Result<FileOutcome> {
    let sub_path = format!("{year:04}/{merchant_slug}-{order_slug}.json");
    let outcome = sink.put(&sub_path, "application/json", body)?;
    Ok(match outcome {
        PutOutcome::Created(url) => FileOutcome::Created(url),
        PutOutcome::Updated(url) => FileOutcome::Updated(url),
    })
}

fn file_blob_to_dir(
    name_stem: &str,
    ext: &str,
    year: i32,
    body: &[u8],
    dir: &Path,
) -> Result<FileOutcome> {
    let target = dir
        .join(format!("{year:04}"))
        .join(format!("{name_stem}.{ext}"));

    let existed = target.exists();
    write_atomic(&target, body)?;

    let label = target.display().to_string();
    if existed {
        info!(target = %label, "receipt blob updated");
        Ok(FileOutcome::Updated(label))
    } else {
        info!(target = %label, "receipt blob created");
        Ok(FileOutcome::Created(label))
    }
}

fn file_blob_to_webdav(
    name_stem: &str,
    ext: &str,
    year: i32,
    body: Vec<u8>,
    sink: &WebdavSink,
) -> Result<FileOutcome> {
    let sub_path = format!("{year:04}/{name_stem}.{ext}");
    let outcome = sink.put(&sub_path, content_type_for(ext), body)?;
    Ok(match outcome {
        PutOutcome::Created(url) => FileOutcome::Created(url),
        PutOutcome::Updated(url) => FileOutcome::Updated(url),
    })
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
            FileOutcome::Updated(_) => panic!("expected Created on first write"),
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
            .file_receipt_blob(&src, "pdf", ("digital-ocean", "inv-42", 2026))
            .unwrap()
            .unwrap();
        let path = match outcome {
            FileOutcome::Created(p) => p,
            FileOutcome::Updated(_) => panic!("expected Created"),
        };
        assert_eq!(
            PathBuf::from(&path),
            tmp.path().join("2026/digital-ocean-inv-42.pdf")
        );
    }
}
