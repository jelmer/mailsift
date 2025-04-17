//! Firefly III bill-registration sink.
//!
//! [Firefly III] is a self-hosted personal finance tracker. Its "bills"
//! concept models recurring expected expenses; exactly the shape of
//! our `.bill.json` artifacts. This sink turns a filed bill into a
//! Firefly bill via the REST API, using update-or-create semantics:
//!
//! - `GET /api/v1/bills?query=<name>`: does a bill with this name
//!   already exist?
//! - If yes, `PUT /api/v1/bills/<id>` to refresh amount/date.
//! - Otherwise `POST /api/v1/bills` to create it.
//!
//! The name is derived from the bill's `payee` field (the local target
//! uses it for the on-disk slug too). Re-running mailsift over the
//! same source mail therefore updates the Firefly record rather than
//! creating duplicates.
//!
//! [Firefly III]: https://www.firefly-iii.org/

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

use super::http_client::{build_client, discard_on_success, ensure_non_empty, json_on_success};

/// Subset of the on-disk `.bill.json` shape we feed to Firefly. Aligns
/// with [`super::bills::Bill`] but kept local so the JSON parsing
/// failure modes are reported with Firefly-specific context.
#[derive(Debug)]
pub struct BillForFirefly<'a> {
    /// Human-readable bill name (Firefly's primary lookup key).
    pub name: &'a str,
    /// Amount as a decimal string ("42.50").
    pub amount: &'a str,
    /// ISO-8601 date string for the next payment due.
    pub date: &'a str,
    /// ISO 4217 currency code ("GBP", "EUR", ...). Optional; Firefly
    /// uses the user's default when missing.
    pub currency_code: Option<&'a str>,
}

pub struct FireflySink {
    client: Client,
    runtime: Handle,
    base_url: String,
    token: String,
}

impl FireflySink {
    pub fn new(base_url: String, token: String, runtime: Handle) -> Result<Self> {
        ensure_non_empty("Firefly base URL", &base_url)?;
        ensure_non_empty("Firefly API token", &token)?;
        let client = build_client("Firefly")?;
        Ok(Self {
            client,
            runtime,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Update an existing Firefly bill with this name, or create a new
    /// one. Returns `Created` / `Updated` for log correlation.
    pub fn register(&self, bill: BillForFirefly<'_>) -> Result<Outcome> {
        self.runtime.block_on(self.register_async(bill))
    }

    async fn register_async(&self, bill: BillForFirefly<'_>) -> Result<Outcome> {
        if let Some(id) = self.find_existing(bill.name).await? {
            debug!(name = bill.name, id, "updating existing Firefly bill");
            self.update(&id, &bill).await?;
            info!(name = bill.name, id, "updated Firefly bill");
            Ok(Outcome::Updated(id))
        } else {
            debug!(name = bill.name, "creating new Firefly bill");
            let id = self.create(&bill).await?;
            info!(name = bill.name, id, "created Firefly bill");
            Ok(Outcome::Created(id))
        }
    }

    /// `GET /api/v1/bills?query=<name>` and look for an exact-name
    /// match in the response. Firefly's `?query=` does a substring
    /// search, so we re-filter client-side to avoid clobbering an
    /// unrelated bill whose name happens to contain ours.
    async fn find_existing(&self, name: &str) -> Result<Option<String>> {
        let url = format!("{}/api/v1/bills", self.base_url);
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .query(&[("query", name)])
            .send()
            .await
            .with_context(|| format!("GET {url}?query={name}"))?;

        let parsed: BillsResponse =
            json_on_success(response, &format!("Firefly GET {url}")).await?;
        for entry in parsed.data {
            if entry.attributes.name == name {
                return Ok(Some(entry.id));
            }
        }
        Ok(None)
    }

    async fn create(&self, bill: &BillForFirefly<'_>) -> Result<String> {
        let url = format!("{}/api/v1/bills", self.base_url);
        let body = BillCreate::from(bill);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        Self::extract_id(url, response).await
    }

    async fn update(&self, id: &str, bill: &BillForFirefly<'_>) -> Result<()> {
        let url = format!("{}/api/v1/bills/{id}", self.base_url);
        let body = BillCreate::from(bill);
        let response = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("PUT {url}"))?;
        discard_on_success(response, &format!("Firefly PUT {url}")).await
    }

    async fn extract_id(url: String, response: reqwest::Response) -> Result<String> {
        let parsed: BillResponse = json_on_success(response, &format!("Firefly {url}")).await?;
        Ok(parsed.data.id)
    }
}

#[derive(Debug)]
pub enum Outcome {
    Created(String),
    Updated(String),
}

/// Fire `register` on a bill against the optional sink, swallowing
/// errors at WARN. Mirrors the `Trackers::register_best_effort`
/// pattern; one external service being down shouldn't affect the
/// on-disk record.
pub fn register_best_effort(sink: Option<&FireflySink>, bill: BillForFirefly<'_>) {
    let Some(sink) = sink else {
        return;
    };
    let name = bill.name.to_string();
    if let Err(e) = sink.register(bill) {
        warn!(
            sink = "firefly",
            name = %name,
            error = %e,
            "Firefly registration failed; on-disk bill record is unaffected"
        );
    }
}

// ---- Firefly API JSON shapes ----------------------------------------------

#[derive(Debug, Serialize)]
struct BillCreate<'a> {
    name: &'a str,
    amount_min: &'a str,
    amount_max: &'a str,
    date: &'a str,
    repeat_freq: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency_code: Option<&'a str>,
}

impl<'a> From<&BillForFirefly<'a>> for BillCreate<'a> {
    fn from(b: &BillForFirefly<'a>) -> Self {
        BillCreate {
            name: b.name,
            // Without lower/upper bounds, treat the bill as a
            // fixed-amount expectation; set both ends to the same
            // value.
            amount_min: b.amount,
            amount_max: b.amount,
            date: b.date,
            // Personal-bills extractors today (utilities, mobile,
            // streaming) are all monthly. If we grow non-monthly bills
            // we can plumb this through.
            repeat_freq: "monthly",
            currency_code: b.currency_code,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BillsResponse {
    data: Vec<BillEntry>,
}

#[derive(Debug, Deserialize)]
struct BillResponse {
    data: BillEntry,
}

#[derive(Debug, Deserialize)]
struct BillEntry {
    id: String,
    attributes: BillAttributes,
}

#[derive(Debug, Deserialize)]
struct BillAttributes {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_url_rejected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match FireflySink::new("".into(), "tok".into(), rt.handle().clone()) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("URL must not be empty"), "{err}");
    }

    #[test]
    fn empty_token_rejected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match FireflySink::new(
            "https://firefly.example.org/".into(),
            "".into(),
            rt.handle().clone(),
        ) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("API token"), "{err}");
    }

    #[test]
    fn trailing_slash_stripped() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let s = FireflySink::new(
            "https://firefly.example.org/".into(),
            "tok".into(),
            rt.handle().clone(),
        )
        .unwrap();
        assert_eq!(s.base_url, "https://firefly.example.org");
    }

    #[test]
    fn bill_create_serialises_with_optional_currency() {
        let bill = BillForFirefly {
            name: "E.ON Next",
            amount: "42.50",
            date: "2026-08-01",
            currency_code: Some("GBP"),
        };
        let json = serde_json::to_string(&BillCreate::from(&bill)).unwrap();
        assert!(json.contains(r#""name":"E.ON Next""#));
        assert!(json.contains(r#""amount_min":"42.50""#));
        assert!(json.contains(r#""amount_max":"42.50""#));
        assert!(json.contains(r#""date":"2026-08-01""#));
        assert!(json.contains(r#""repeat_freq":"monthly""#));
        assert!(json.contains(r#""currency_code":"GBP""#));
    }

    #[test]
    fn bill_create_omits_missing_currency() {
        let bill = BillForFirefly {
            name: "Subscription",
            amount: "9.99",
            date: "2026-08-01",
            currency_code: None,
        };
        let json = serde_json::to_string(&BillCreate::from(&bill)).unwrap();
        assert!(!json.contains("currency_code"), "{json}");
    }
}
