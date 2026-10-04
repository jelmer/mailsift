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
//! Firefly side, so re-runs idempotently refresh the record). Firefly
//! keeps one bill per payee, so a bill is only registered while no
//! bill for that payee from a newer message is on file.
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
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::{debug, warn};

use super::FileOutcome;
use super::firefly::{self, BillForFirefly, FireflySink};
use super::json_target::{
    derive_year, filed_from_newer, first_non_empty, is_from_newer, read_and_parse, received_at_in,
    write_unless_newer,
};
use super::sink::{sanitize_ext, slugify, update_file};

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
    let (body, bill) = read_and_parse::<Bill>(src, "bill")?;

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

    let body_out = super::json_target::body_with_received_at(&body, received_at_epoch);
    let outcome = write_unless_newer(&target, &body_out, "bill")?;
    if matches!(outcome, FileOutcome::Kept(_)) {
        return Ok(outcome);
    }

    // Firefly keeps one bill per payee, not one per invoice, so an
    // older invoice must not overwrite what a newer one put there.
    if firefly.is_some() {
        let incoming = received_at_in(body_out.as_bytes())?;
        match payee_has_newer_bill(dir, &payee_slug, incoming) {
            Ok(false) => {}
            Ok(true) => {
                debug!(payee, "a newer bill is on file; leaving Firefly alone");
                return Ok(outcome);
            }
            Err(e) => {
                warn!(
                    payee,
                    error = format!("{e:#}"),
                    "can't tell whether a newer bill is on file; leaving Firefly alone"
                );
                return Ok(outcome);
            }
        }
    }

    // Best-effort Firefly registration. We try on every filing (not
    // just on creation) because the Firefly side does its own
    // update-or-create; an "update" here genuinely needs to refresh
    // the bill's amount and due-date on the Firefly server too.
    register_with_firefly(firefly, payee, &bill);

    Ok(outcome)
}

/// Whether any bill on file for the payee came from a newer message
/// than one dated `incoming`, whichever invoice or year it is filed
/// under.
fn payee_has_newer_bill(
    dir: &Path,
    payee_slug: &str,
    incoming: Option<DateTime<Utc>>,
) -> Result<bool> {
    let prefix = format!("{payee_slug}-");
    for year_dir in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let year_dir = year_dir?.path();
        if !year_dir.is_dir() {
            continue;
        }
        let entries =
            fs::read_dir(&year_dir).with_context(|| format!("listing {}", year_dir.display()))?;
        for entry in entries {
            let path = entry?.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                continue;
            }
            let body = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            // The prefix also matches a payee whose name merely starts
            // the same way ("Acme" and "Acme Corp"); go by the record.
            let same_payee = serde_json::from_slice::<Bill>(&body)
                .ok()
                .and_then(|bill| bill.payee().map(|payee| slugify(payee, false)))
                .is_some_and(|slug| slug == payee_slug);
            if same_payee && is_from_newer(&body, &name, incoming) {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
///
/// `received_at` is the date that sibling was filed under. A blob has
/// no date of its own, so it follows the JSON: when the record on file
/// came from a newer message, the blob on file did too and is kept.
pub fn file_bill_blob(
    src: &Path,
    ext: &str,
    pair: (&str, &str, i32),
    dir: &Path,
    received_at: Option<DateTime<Utc>>,
) -> Result<FileOutcome> {
    let ext = sanitize_ext(ext)?;
    let (payee, invoice, year) = pair;
    let name_stem = format!("{payee}-{invoice}");

    let year_dir = dir.join(format!("{year:04}"));
    let target = year_dir.join(format!("{name_stem}.{ext}"));
    let record = year_dir.join(format!("{name_stem}.json"));
    let body = fs::read(src).with_context(|| format!("reading bill blob {}", src.display()))?;

    update_file(&target, "bill blob", &|_| {
        Ok((!filed_from_newer(&record, received_at)?).then(|| body.clone()))
    })
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

        let outcome =
            file_bill_blob(&src, "pdf", ("acme-corp", "inv-42", 2024), &dir, None).unwrap();
        let path = match outcome {
            FileOutcome::Created(p) => p,
            other => panic!("expected Created, got {other:?}"),
        };
        assert_eq!(
            std::path::PathBuf::from(&path),
            dir.join("2024/acme-corp-inv-42.pdf")
        );
    }

    // 2024-11-01T00:00:00Z and a reminder sent two weeks later.
    const INVOICE_SENT: i64 = 1730419200;
    const REMINDER_SENT: i64 = 1731628800;

    fn write_bill(path: &Path, total: f64) {
        let body = serde_json::json!({
            "payee": "Acme", "invoiceNumber": "INV1", "dueDate": "2024-12-05", "total": total,
        });
        fs::write(path, body.to_string()).unwrap();
    }

    #[test]
    fn older_mail_does_not_replace_bill_or_its_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bill.json");
        let blob = tmp.path().join("bill.pdf");
        let dir = tmp.path().join("out");
        let pair = ("acme", "inv1", 2024);
        let at = |epoch| DateTime::from_timestamp(epoch, 0);

        write_bill(&src, 120.0);
        fs::write(&blob, b"reminder").unwrap();
        file_bill(&src, &dir, None, Some(REMINDER_SENT)).unwrap();
        file_bill_blob(&blob, "pdf", pair, &dir, at(REMINDER_SENT)).unwrap();

        write_bill(&src, 100.0);
        fs::write(&blob, b"invoice").unwrap();
        let outcome = file_bill(&src, &dir, None, Some(INVOICE_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Kept(_)));
        let outcome = file_bill_blob(&blob, "pdf", pair, &dir, at(INVOICE_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Kept(_)));

        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("2024/acme-inv1.json")).unwrap()).unwrap();
        assert_eq!(v["total"], 120.0);
        assert_eq!(
            fs::read(dir.join("2024/acme-inv1.pdf")).unwrap(),
            b"reminder"
        );
    }

    #[test]
    fn payee_has_newer_bill_looks_across_invoices_and_years() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bill.json");
        let dir = tmp.path().join("out");
        let at = |epoch| DateTime::from_timestamp(epoch, 0);
        let file = |payee: &str, invoice: &str, due: &str, sent: i64| {
            let body =
                serde_json::json!({"payee": payee, "invoiceNumber": invoice, "dueDate": due});
            fs::write(&src, body.to_string()).unwrap();
            file_bill(&src, &dir, None, Some(sent)).unwrap();
        };

        file("Acme", "INV1", "2024-12-05", INVOICE_SENT);
        assert!(!payee_has_newer_bill(&dir, "acme", at(INVOICE_SENT)).unwrap());
        assert!(!payee_has_newer_bill(&dir, "acme", at(REMINDER_SENT)).unwrap());

        // A later invoice for the same payee, filed under another year.
        file("Acme", "INV2", "2025-01-05", REMINDER_SENT);
        assert!(payee_has_newer_bill(&dir, "acme", at(INVOICE_SENT)).unwrap());
        assert!(!payee_has_newer_bill(&dir, "acme", at(REMINDER_SENT)).unwrap());
    }

    /// Firefly keeps one bill per payee. An invoice older than one
    /// already filed for that payee is a record of its own on disk,
    /// but must not reach Firefly.
    #[test]
    fn older_invoice_is_not_registered_with_firefly() {
        use crate::targets::fake_dav::{FakeDav, runtime_handle};

        let server = FakeDav::start();
        let firefly =
            FireflySink::new(server.base_url.clone(), "token".into(), runtime_handle()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bill.json");
        let dir = tmp.path().join("out");
        let file = |invoice: &str, due: &str, sent: i64| {
            let body = serde_json::json!({
                "payee": "Acme", "invoiceNumber": invoice, "dueDate": due,
                "totalPaymentDue": {"price": 42.5, "priceCurrency": "GBP"},
            });
            fs::write(&src, body.to_string()).unwrap();
            file_bill(&src, &dir, Some(&firefly), Some(sent)).unwrap()
        };
        const LOOKUP: &str = "GET /api/v1/bills?query=Acme";

        file("INV2", "2025-01-05", REMINDER_SENT);
        assert_eq!(server.requests(), vec![LOOKUP]);

        let outcome = file("INV1", "2024-12-05", INVOICE_SENT);
        assert!(matches!(outcome, FileOutcome::Created(_)));
        assert_eq!(server.requests(), vec![LOOKUP]);

        file("INV3", "2025-02-05", REMINDER_SENT + 86400);
        assert_eq!(server.requests(), vec![LOOKUP, LOOKUP]);
    }

    #[test]
    fn payee_has_newer_bill_ignores_other_payees() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bill.json");
        let dir = tmp.path().join("out");
        let body = serde_json::json!({
            "payee": "Acme Corp", "invoiceNumber": "INV9", "dueDate": "2024-12-05",
        });
        fs::write(&src, body.to_string()).unwrap();
        file_bill(&src, &dir, None, Some(REMINDER_SENT)).unwrap();

        let invoice_sent = DateTime::from_timestamp(INVOICE_SENT, 0);
        assert!(!payee_has_newer_bill(&dir, "acme", invoice_sent).unwrap());
        assert!(payee_has_newer_bill(&dir, "acme-corp", invoice_sent).unwrap());
    }

    #[test]
    fn newer_mail_replaces_bill_and_its_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bill.json");
        let blob = tmp.path().join("bill.pdf");
        let dir = tmp.path().join("out");
        let pair = ("acme", "inv1", 2024);
        let at = |epoch| DateTime::from_timestamp(epoch, 0);

        write_bill(&src, 100.0);
        fs::write(&blob, b"invoice").unwrap();
        file_bill(&src, &dir, None, Some(INVOICE_SENT)).unwrap();
        file_bill_blob(&blob, "pdf", pair, &dir, at(INVOICE_SENT)).unwrap();

        write_bill(&src, 120.0);
        fs::write(&blob, b"reminder").unwrap();
        let outcome = file_bill(&src, &dir, None, Some(REMINDER_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));
        let outcome = file_bill_blob(&blob, "pdf", pair, &dir, at(REMINDER_SENT)).unwrap();
        assert!(matches!(outcome, FileOutcome::Updated(_)));

        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("2024/acme-inv1.json")).unwrap()).unwrap();
        assert_eq!(v["total"], 120.0);
        assert_eq!(
            fs::read(dir.join("2024/acme-inv1.pdf")).unwrap(),
            b"reminder"
        );
    }
}
