//! Local-directory target for `bill` artifacts.
//!
//! Each `.bill.json` artifact is parsed for its `payee` and
//! `invoiceNumber` (loosely schema.org `Invoice`-shaped), and filed
//! under `<dir>/<year>/<payee_slug>-<invoiceNumber>.json`. The year
//! comes from the `dueDate` field if present, otherwise the message
//! `Date:` header. Extractors are expected to populate `dueDate`, but
//! the fallback keeps us from blowing up on partial input.
//!
//! When a [`super::firefly::FireflySink`] is configured, every filed
//! bill is also registered with Firefly III (update-or-create on the
//! Firefly side, so re-runs idempotently refresh the record).
//!
//! Extractors may also emit companion blobs (`<slug>.bill.pdf` etc.).
//! Those go through [`file_bill_blob`], which requires a same-slug
//! `.bill.json` sibling in the same run and files the pair under a
//! common `<payee>-<invoice>` name. Blobs without a sibling are
//! dropped with a warning by the pipeline: the extractor should have
//! emitted the structured record too.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::FileOutcome;
use super::firefly::{self, BillForFirefly, FireflySink};
use super::json_target::{derive_year, first_non_empty};
use super::sink::{log_file_outcome, sanitize_ext, slugify, write_atomic};

/// Shape we read out of a `.bill.json` artifact. Loosely schema.org
/// `Invoice`-shaped; unknown fields are ignored so extractors can emit
/// richer JSON without breaking the target.
#[derive(Debug, Deserialize)]
struct Bill {
    payee: Option<String>,
    #[serde(rename = "accountName")]
    account_name: Option<String>,
    #[serde(rename = "invoiceNumber")]
    invoice_number: Option<String>,
    identifier: Option<String>,
    #[serde(rename = "dueDate")]
    due_date: Option<String>,
    #[serde(rename = "paymentDueDate")]
    payment_due_date: Option<String>,
    date: Option<String>,
    #[serde(rename = "issueDate")]
    issue_date: Option<String>,
    /// schema.org `PriceSpecification` carrying amount + currency.
    /// Required for Firefly registration; the local target works
    /// without it.
    #[serde(rename = "totalPaymentDue")]
    total_payment_due: Option<PriceSpecification>,
}

#[derive(Debug, Deserialize)]
struct PriceSpecification {
    price: Option<serde_json::Number>,
    #[serde(rename = "priceCurrency")]
    price_currency: Option<String>,
}

impl Bill {
    fn payee(&self) -> Option<&str> {
        first_non_empty([self.payee.as_deref(), self.account_name.as_deref()])
    }

    fn invoice(&self) -> Option<&str> {
        first_non_empty([self.invoice_number.as_deref(), self.identifier.as_deref()])
    }

    fn date_candidates(&self) -> [Option<&str>; 4] {
        [
            self.due_date.as_deref(),
            self.payment_due_date.as_deref(),
            self.date.as_deref(),
            self.issue_date.as_deref(),
        ]
    }
}

pub fn file_bill(
    src: &Path,
    dir: &Path,
    firefly: Option<&FireflySink>,
    received_at_epoch: Option<i64>,
) -> Result<FileOutcome> {
    let body = fs::read_to_string(src)
        .with_context(|| format!("reading bill source {}", src.display()))?;
    let bill: Bill = serde_json::from_str(&body)
        .with_context(|| format!("parsing bill JSON {}", src.display()))?;

    let payee = bill
        .payee()
        .ok_or_else(|| anyhow!("{}: missing 'payee'", src.display()))?;
    let invoice = bill
        .invoice()
        .ok_or_else(|| anyhow!("{}: missing 'invoiceNumber'", src.display()))?;
    let year = derive_year(bill.date_candidates());

    let payee_slug = slugify(payee, false);
    let invoice_slug = slugify(invoice, false);
    if payee_slug.is_empty() || invoice_slug.is_empty() {
        bail!(
            "{}: empty slug after sanitisation (payee={payee:?} invoice={invoice:?})",
            src.display()
        );
    }

    let target = dir
        .join(format!("{year:04}"))
        .join(format!("{payee_slug}-{invoice_slug}.json"));

    let existed = target.exists();
    let body_out = super::json_target::body_with_received_at(&body, received_at_epoch);
    write_atomic(&target, body_out.as_bytes())?;

    // Best-effort Firefly registration. We try on every filing (not
    // just on creation) because the Firefly side does its own
    // update-or-create; an "update" here genuinely needs to refresh
    // the bill's amount and due-date on the Firefly server too.
    register_with_firefly(firefly, payee, &bill);

    Ok(log_file_outcome(&target, existed, "bill"))
}

/// Translate a parsed [`Bill`] to a [`BillForFirefly`] and fire the
/// best-effort registration. Skips silently when the required fields
/// (amount + due date) are missing; Firefly needs both, and not every
/// extractor surfaces them.
fn register_with_firefly(sink: Option<&FireflySink>, payee: &str, bill: &Bill) {
    let Some(sink) = sink else {
        return;
    };
    let Some(price) = bill
        .total_payment_due
        .as_ref()
        .and_then(|p| p.price.as_ref())
    else {
        return;
    };
    let due = match first_non_empty([bill.due_date.as_deref(), bill.payment_due_date.as_deref()]) {
        Some(d) => d,
        None => return,
    };
    let currency = bill
        .total_payment_due
        .as_ref()
        .and_then(|p| p.price_currency.as_deref());
    let amount = price.to_string();
    firefly::register_best_effort(
        Some(sink),
        BillForFirefly {
            name: payee,
            amount: &amount,
            date: due,
            currency_code: currency,
        },
    );
}

/// File a companion blob (typically a PDF) alongside a bill.
///
/// `pair` is the sanitised `(payee_slug, invoice_slug, year)` pulled
/// from a same-slug `.bill.json` sibling in the same extractor run;
/// the blob is filed under `<year>/<payee>-<invoice>.<ext>` so it sits
/// beside the JSON.
pub fn file_bill_blob(
    src: &Path,
    ext: &str,
    pair: (&str, &str, i32),
    dir: &Path,
) -> Result<FileOutcome> {
    let ext = sanitize_ext(ext)?;
    let (payee, invoice, year) = pair;
    let name_stem = format!("{payee}-{invoice}");

    let target = dir
        .join(format!("{year:04}"))
        .join(format!("{name_stem}.{ext}"));

    let body = fs::read(src).with_context(|| format!("reading bill blob {}", src.display()))?;
    let existed = target.exists();
    write_atomic(&target, &body)?;

    Ok(log_file_outcome(&target, existed, "bill blob"))
}

/// Parse `body` as a bill JSON and return the paired
/// `(payee_slug, invoice_slug, year)` that a companion blob should be
/// filed under, or `None` if a required field is missing.
///
/// Used by the pipeline to resolve a sibling `.bill.json` to the name
/// a same-slug `.bill.<ext>` blob should be filed under.
pub fn paired_name_from_json(body: &str) -> Option<(String, String, i32)> {
    let bill: Bill = serde_json::from_str(body).ok()?;
    let payee_slug = slugify(bill.payee()?, false);
    let invoice_slug = slugify(bill.invoice()?, false);
    if payee_slug.is_empty() || invoice_slug.is_empty() {
        return None;
    }
    let year = derive_year(bill.date_candidates());
    Some((payee_slug, invoice_slug, year))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_from_due_date() {
        let bill: Bill = serde_json::from_value(serde_json::json!({"dueDate": "2024-12-05"}))
            .expect("valid bill");
        assert_eq!(derive_year(bill.date_candidates()), 2024);
    }

    #[test]
    fn file_bill_stamps_received_at() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bill.json");
        std::fs::write(
            &src,
            br#"{"payee":"Acme","invoiceNumber":"INV1","dueDate":"2024-12-05"}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");
        std::fs::create_dir_all(&dir).unwrap();
        // 2024-11-01T00:00:00Z
        file_bill(&src, &dir, None, Some(1730419200)).unwrap();
        let body = std::fs::read_to_string(dir.join("2024/acme-inv1.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["receivedAt"], "2024-11-01T00:00:00Z");
    }

    #[test]
    fn paired_name_from_json_derives_slugs_and_year() {
        let body = r#"{"payee":"Acme Corp","invoiceNumber":"INV-42","dueDate":"2024-12-05"}"#;
        let (payee, invoice, year) = paired_name_from_json(body).unwrap();
        assert_eq!(payee, "acme-corp");
        assert_eq!(invoice, "inv-42");
        assert_eq!(year, 2024);
    }

    #[test]
    fn blob_paired_lands_beside_json() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("blob.pdf");
        std::fs::write(&src, b"%PDF-1.4 fake").unwrap();
        let dir = tmp.path().join("out");

        let outcome = file_bill_blob(&src, "pdf", ("acme-corp", "inv-42", 2024), &dir).unwrap();
        let path = match outcome {
            FileOutcome::Created(p) => p,
            FileOutcome::Updated(_) => panic!("expected Created"),
        };
        assert_eq!(
            std::path::PathBuf::from(&path),
            dir.join("2024/acme-corp-inv-42.pdf")
        );
    }
}
