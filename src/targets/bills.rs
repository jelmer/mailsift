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

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tracing::info;

use super::FileOutcome;
use super::firefly::{self, BillForFirefly, FireflySink};
use super::json_target::{derive_year, first_non_empty};
use super::sink::{slugify, write_atomic};

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

pub fn file_bill(src: &Path, dir: &Path, firefly: Option<&FireflySink>) -> Result<FileOutcome> {
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
    write_atomic(&target, body.as_bytes())?;

    // Best-effort Firefly registration. We try on every filing (not
    // just on creation) because the Firefly side does its own
    // update-or-create; an "update" here genuinely needs to refresh
    // the bill's amount and due-date on the Firefly server too.
    register_with_firefly(firefly, payee, &bill);

    let label = target.display().to_string();
    if existed {
        info!(target = %label, "bill updated");
        Ok(FileOutcome::Updated(label))
    } else {
        info!(target = %label, "bill created");
        Ok(FileOutcome::Created(label))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_from_due_date() {
        let bill: Bill = serde_json::from_value(serde_json::json!({"dueDate": "2024-12-05"}))
            .expect("valid bill");
        assert_eq!(derive_year(bill.date_candidates()), 2024);
    }
}
