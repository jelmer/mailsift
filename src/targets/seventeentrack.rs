//! 17track tracker-registration sink.
//!
//! `POST https://api.17track.net/track/v2.2/register` with a
//! `17token` header and a JSON array of `{number, carrier}` items.
//! 17track then polls the carrier API for status updates.
//!
//! Carrier translation: 17track uses small integer codes per carrier
//! (`100001` = Royal Mail, `100008` = DPD UK, etc.). We translate our
//! canonical carrier IDs (`royal-mail`, `dpd`, ...) at the sink
//! boundary.

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::Serialize;
use tokio::runtime::Handle;
use tracing::info;

use super::http_client::{body_on_success, build_client, ensure_non_empty, truncate};
use super::trackers::TrackerSink;

const DEFAULT_API_URL: &str = "https://api.17track.net/track/v2.2/register";

/// Map our canonical carrier IDs to 17track's numeric carrier codes.
///
/// See https://api.17track.net/en/carrier-codes for the upstream
/// reference list.
fn to_seventeentrack(carrier_id: &str) -> Option<u32> {
    match carrier_id {
        "royal-mail" => Some(100001),
        "dpd" => Some(100008),
        _ => None,
    }
}

#[derive(Debug)]
pub struct SeventeenTrackClient {
    client: Client,
    runtime: Handle,
    api_url: String,
    token: String,
}

impl SeventeenTrackClient {
    pub fn new(token: String, runtime: Handle) -> Result<Self> {
        Self::with_url(DEFAULT_API_URL.to_string(), token, runtime)
    }

    pub fn with_url(api_url: String, token: String, runtime: Handle) -> Result<Self> {
        ensure_non_empty("17track API URL", &api_url)?;
        ensure_non_empty("17track API token", &token)?;
        let client = build_client("17track")?;
        Ok(Self {
            client,
            runtime,
            api_url,
            token,
        })
    }

    async fn register_async(&self, carrier_code: u32, tracking_number: &str) -> Result<()> {
        let body = [TrackItem {
            number: tracking_number,
            carrier: carrier_code,
        }];
        let response = self
            .client
            .post(&self.api_url)
            .header("17token", &self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", self.api_url))?;

        // 17track always returns HTTP 200, even for application-level
        // errors. We still let `body_on_success` handle the transport
        // case (5xx, etc.) before parsing the application-level code.
        let op_label = format!("17track POST {}", self.api_url);
        let body_text = body_on_success(response, &op_label).await?;
        let parsed: ApiResponse = serde_json::from_str(&body_text)
            .with_context(|| format!("parsing 17track response: {}", truncate(&body_text, 200)))?;

        if parsed.code != 0 {
            return Err(anyhow!(
                "17track registration returned code {} ({})",
                parsed.code,
                parsed
                    .data
                    .as_ref()
                    .and_then(|d| d.errors.first().map(|e| e.message.as_str()))
                    .unwrap_or("no message")
            ));
        }

        info!(
            tracking = %tracking_number,
            carrier = carrier_code,
            "registered tracker with 17track"
        );
        Ok(())
    }
}

impl TrackerSink for SeventeenTrackClient {
    fn name(&self) -> &str {
        "17track"
    }

    fn register(&self, carrier_id: &str, tracking_number: &str) -> Result<()> {
        let code = to_seventeentrack(carrier_id)
            .ok_or_else(|| anyhow!("no 17track carrier mapping for {carrier_id:?}"))?;
        self.runtime
            .block_on(self.register_async(code, tracking_number))
    }
}

#[derive(Serialize)]
struct TrackItem<'a> {
    number: &'a str,
    carrier: u32,
}

#[derive(serde::Deserialize)]
struct ApiResponse {
    code: i32,
    #[serde(default)]
    data: Option<ApiData>,
}

#[derive(serde::Deserialize)]
struct ApiData {
    #[serde(default)]
    errors: Vec<ApiError>,
}

#[derive(serde::Deserialize)]
struct ApiError {
    message: String,
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
    fn empty_token_rejected() {
        let err = SeventeenTrackClient::new(String::new(), test_handle()).unwrap_err();
        assert!(err.to_string().contains("token"));
    }

    #[test]
    fn carrier_mapping() {
        assert_eq!(to_seventeentrack("royal-mail"), Some(100001));
        assert_eq!(to_seventeentrack("dpd"), Some(100008));
        assert_eq!(to_seventeentrack("unknown"), None);
    }
}
