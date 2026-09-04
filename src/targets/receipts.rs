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

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tracing::info;

use super::FileOutcome;
use super::json_target::{derive_year, first_non_empty};
use super::mail_forward::{self, MailForwarder};
use super::sink::{sanitize_ext, slugify, write_atomic};
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

/// The three fields we derive from a `.receipt.json` to build its
/// on-disk path.
struct DerivedFilename {
    merchant_slug: String,
    order_slug: String,
    year: i32,
}

/// Read `.receipt.json` at `src` and derive the filename components
/// used for both the JSON itself and any paired binary sidecar. The
/// body is returned so callers that need the raw JSON (e.g. the
/// received-at rewrite) don't re-read the file.
fn derive_from_receipt_json(src: &Path) -> Result<(String, DerivedFilename)> {
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
    Ok((
        body,
        DerivedFilename {
            merchant_slug,
            order_slug,
            year,
        },
    ))
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
        let (body, derived) = derive_from_receipt_json(src)?;
        let DerivedFilename {
            merchant_slug,
            order_slug,
            year,
        } = derived;

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

    /// File a binary sidecar (typically the original PDF) that
    /// belongs to the receipt described by `sibling_json`.
    ///
    /// The path is `<year>/<merchant>-<order>.<ext>`, sharing the
    /// merchant/order/year with the JSON so the two files land next to
    /// each other. `ext` is validated with the same sanitiser tickets
    /// use.
    ///
    /// The [`ReceiptSink::Forward`] variant is a no-op: the original
    /// message (which already carries the attachment) has been
    /// forwarded by [`Self::file_receipt`], and re-forwarding once per
    /// artifact would multiply mail volume.
    pub fn file_receipt_file(
        &self,
        src: &Path,
        sibling_json: &Path,
        ext: &str,
    ) -> Result<Option<FileOutcome>> {
        let sanitized_ext = sanitize_ext(ext)?;
        let (_body, derived) = derive_from_receipt_json(sibling_json)?;
        let DerivedFilename {
            merchant_slug,
            order_slug,
            year,
        } = derived;

        match self {
            ReceiptSink::LocalDir(dir) => {
                let bytes = fs::read(src)
                    .with_context(|| format!("reading receipt file {}", src.display()))?;
                Ok(Some(receipt_file_to_dir(
                    &merchant_slug,
                    &order_slug,
                    year,
                    &sanitized_ext,
                    &bytes,
                    dir,
                )?))
            }
            ReceiptSink::Webdav(sink) => {
                let bytes = fs::read(src)
                    .with_context(|| format!("reading receipt file {}", src.display()))?;
                Ok(Some(receipt_file_to_webdav(
                    &merchant_slug,
                    &order_slug,
                    year,
                    &sanitized_ext,
                    bytes,
                    sink,
                )?))
            }
            ReceiptSink::Forward(_) => Ok(None),
        }
    }
}

fn receipt_file_to_dir(
    merchant_slug: &str,
    order_slug: &str,
    year: i32,
    ext: &str,
    body: &[u8],
    dir: &Path,
) -> Result<FileOutcome> {
    let target = dir
        .join(format!("{year:04}"))
        .join(format!("{merchant_slug}-{order_slug}.{ext}"));

    let existed = target.exists();
    write_atomic(&target, body)?;

    if existed {
        info!(target = %target.display(), "receipt file updated");
        Ok(FileOutcome::Updated(target.display().to_string()))
    } else {
        info!(target = %target.display(), "receipt file created");
        Ok(FileOutcome::Created(target.display().to_string()))
    }
}

fn receipt_file_to_webdav(
    merchant_slug: &str,
    order_slug: &str,
    year: i32,
    ext: &str,
    body: Vec<u8>,
    sink: &WebdavSink,
) -> Result<FileOutcome> {
    let sub_path = format!("{year:04}/{merchant_slug}-{order_slug}.{ext}");
    let content_type = mime_for_ext(ext);
    let outcome = sink.put(&sub_path, content_type, body)?;
    Ok(match outcome {
        PutOutcome::Created(url) => FileOutcome::Created(url),
        PutOutcome::Updated(url) => FileOutcome::Updated(url),
    })
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
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
    fn local_dir_files_receipt_file_next_to_json() {
        let tmp = tempfile::tempdir().unwrap();
        let json = tmp.path().join("receipt.json");
        std::fs::write(
            &json,
            br#"{"merchant": "Amazon", "orderNumber": "ABC123", "orderDate": "2024-08-10"}"#,
        )
        .unwrap();
        let pdf = tmp.path().join("attachment.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 fake").unwrap();

        let out = tempfile::tempdir().unwrap();
        let sink = ReceiptSink::LocalDir(out.path().to_path_buf());
        let outcome = sink.file_receipt_file(&pdf, &json, "pdf").unwrap().unwrap();
        match outcome {
            FileOutcome::Created(p) => {
                let expected = out.path().join("2024/amazon-abc123.pdf");
                assert_eq!(PathBuf::from(p), expected);
                assert_eq!(std::fs::read(&expected).unwrap(), b"%PDF-1.4 fake");
            }
            FileOutcome::Updated(_) => panic!("expected Created on first write"),
        }
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
}
