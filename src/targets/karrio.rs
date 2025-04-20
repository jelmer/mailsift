//! Karrio tracker-registration sink.
//!
//! `POST /v1/trackers` with `tracking_number` + `carrier_name`. Karrio
//! then polls the carrier API for status updates. Idempotent on the
//! server side: re-registering the same pair returns the existing
//! tracker.
//!
//! Carrier translation: extractors emit our canonical IDs
//! (`royal-mail`, `dpd`, ...); this sink maps them to whatever string
//! Karrio's API expects. Unmapped carriers are logged and skipped.

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::Serialize;
use tokio::runtime::Handle;
use tracing::info;

use super::http_client::{build_client, discard_on_success, ensure_non_empty};
use super::trackers::TrackerSink;

/// Map our canonical carrier IDs to Karrio's `carrier_name` values.
///
/// Karrio's identifiers happen to mostly match ours (lowercase, no
/// hyphens), but they're not guaranteed to; see e.g. their use of
/// `dpd_uk` to distinguish DPD UK from DPD Germany.
fn to_karrio(carrier_id: &str) -> Option<&'static str> {
    match carrier_id {
        "royal-mail" => Some("royalmail"),
        "dpd" => Some("dpd_uk"),
        _ => None,
    }
}

#[derive(Debug)]
pub struct KarrioClient {
    client: Client,
    runtime: Handle,
    base_url: String,
    token: String,
}

impl KarrioClient {
    pub fn new(base_url: String, token: String, runtime: Handle) -> Result<Self> {
        ensure_non_empty("Karrio base URL", &base_url)?;
        ensure_non_empty("Karrio API token", &token)?;
        let client = build_client("Karrio")?;
        Ok(Self {
            client,
            runtime,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    async fn register_async(&self, carrier_name: &str, tracking_number: &str) -> Result<()> {
        let url = format!("{}/v1/trackers", self.base_url);
        let body = TrackerRequest {
            tracking_number,
            carrier_name,
        };
        let response = self
            .client
            .post(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Token {}", self.token),
            )
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        discard_on_success(response, &format!("Karrio POST {url}")).await?;
        info!(
            tracking = %tracking_number,
            carrier = %carrier_name,
            "registered tracker with Karrio"
        );
        Ok(())
    }
}

impl TrackerSink for KarrioClient {
    fn name(&self) -> &str {
        "karrio"
    }

    fn register(&self, carrier_id: &str, tracking_number: &str) -> Result<()> {
        let karrio_carrier = to_karrio(carrier_id)
            .ok_or_else(|| anyhow!("no Karrio carrier mapping for {carrier_id:?}"))?;
        self.runtime
            .block_on(self.register_async(karrio_carrier, tracking_number))
    }
}

#[derive(Serialize)]
struct TrackerRequest<'a> {
    tracking_number: &'a str,
    carrier_name: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use tokio::runtime::Runtime;

    fn test_handle() -> Handle {
        static RT: OnceLock<Runtime> = OnceLock::new();
        RT.get_or_init(|| Runtime::new().unwrap()).handle().clone()
    }

    #[test]
    fn empty_url_rejected() {
        let err = KarrioClient::new(String::new(), "tok".into(), test_handle()).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn empty_token_rejected() {
        let err = KarrioClient::new("https://k.example".into(), String::new(), test_handle())
            .unwrap_err();
        assert!(err.to_string().contains("token"));
    }

    #[test]
    fn trailing_slash_stripped() {
        let client = KarrioClient::new(
            "https://karrio.example.org/".into(),
            "tok".into(),
            test_handle(),
        )
        .unwrap();
        assert_eq!(client.base_url, "https://karrio.example.org");
    }

    #[test]
    fn carrier_mapping() {
        assert_eq!(to_karrio("royal-mail"), Some("royalmail"));
        assert_eq!(to_karrio("dpd"), Some("dpd_uk"));
        assert_eq!(to_karrio("unknown"), None);
    }
}
