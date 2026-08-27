//! Local-directory target for `parcel` artifacts.
//!
//! Each `.parcel.json` artifact is parsed for its `trackingNumber` and
//! filed under `<dir>/<trackingNumber>.json`. Unlike bills, parcels
//! merge: if a file already exists at the target path we overlay the
//! incoming fields onto it and append a `history` entry so the on-disk
//! record gets richer as a parcel progresses ("on its way" →
//! "out for delivery" → "delivered").

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::info;

use super::FileOutcome;
use super::sink::{slugify, write_atomic};

/// Shape we read out of a `.parcel.json` artifact. Loosely schema.org
/// `ParcelDelivery`-shaped; unknown fields pass through unchanged.
#[derive(Debug, Deserialize)]
struct Parcel {
    #[serde(rename = "trackingNumber")]
    tracking_number: Option<String>,
    identifier: Option<String>,
    provider: Option<Provider>,
}

/// The `provider` sub-object. `@id` carries our canonical carrier
/// identifier (`royal-mail`, `dpd`, ...); each tracker sink translates
/// it to whatever its upstream service expects.
#[derive(Debug, Deserialize)]
struct Provider {
    #[serde(rename = "@id")]
    id: Option<String>,
}

impl Parcel {
    fn tracking(&self) -> Option<&str> {
        for candidate in [&self.tracking_number, &self.identifier] {
            if let Some(s) = candidate.as_deref() {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        None
    }

    fn carrier_id(&self) -> Option<&str> {
        let id = self.provider.as_ref()?.id.as_deref()?.trim();
        if id.is_empty() { None } else { Some(id) }
    }
}

pub fn file_parcel(
    src: &Path,
    dir: &Path,
    trackers: Option<&super::trackers::Trackers>,
    received_at_epoch: Option<i64>,
) -> Result<FileOutcome> {
    let body = fs::read_to_string(src)
        .with_context(|| format!("reading parcel source {}", src.display()))?;

    let parcel: Parcel = serde_json::from_str(&body)
        .with_context(|| format!("parsing parcel JSON {}", src.display()))?;
    let tracking = parcel
        .tracking()
        .ok_or_else(|| anyhow!("{}: missing 'trackingNumber'", src.display()))?;

    let tracking_slug = slugify(tracking, true);
    if tracking_slug.is_empty() {
        bail!(
            "{}: empty slug after sanitisation (trackingNumber={tracking:?})",
            src.display()
        );
    }

    let mut incoming: Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing parcel JSON {}", src.display()))?;
    // Stamp incoming with `receivedAt` before merge/init so that the
    // history entry we mint from it inherits the same timestamp.
    if let Some(epoch) = received_at_epoch
        && let Some(stamped) = super::json_target::format_received_at(epoch)
        && let Some(obj) = incoming.as_object_mut()
        && !obj.contains_key("receivedAt")
    {
        obj.insert("receivedAt".into(), Value::String(stamped));
    }
    // Synthesise a trackingUrl for well-known carriers when the
    // extractor didn't supply one. Extractor-provided URLs always win.
    if let Some(obj) = incoming.as_object_mut()
        && !obj.contains_key("trackingUrl")
        && let Some(carrier) = parcel.carrier_id()
        && let Some(url) = tracking_url_for(carrier, tracking)
    {
        obj.insert("trackingUrl".into(), Value::String(url));
    }

    let target = dir.join(format!("{tracking_slug}.json"));
    let existed = target.exists();

    let merged = if existed {
        let existing_body = fs::read_to_string(&target)
            .with_context(|| format!("reading existing parcel {}", target.display()))?;
        let existing: Value = serde_json::from_str(&existing_body)
            .with_context(|| format!("parsing existing parcel {}", target.display()))?;
        merge(existing, incoming)
    } else {
        with_initial_history(incoming)
    };

    let serialised = serde_json::to_vec_pretty(&merged).context("serialising merged parcel")?;
    write_atomic(&target, &serialised)?;

    let label = target.display().to_string();
    if existed {
        info!(target = %label, "parcel updated");
        Ok(FileOutcome::Updated(label))
    } else {
        info!(target = %label, "parcel created");
        // First time we've seen this tracking number; fan out to every
        // configured tracker registration sink so they can start
        // polling the carrier. Silently skip parcels with no
        // `provider.@id`; the on-disk record stands on its own.
        if let Some(trackers) = trackers
            && !trackers.is_empty()
            && let Some(carrier) = parcel.carrier_id()
        {
            trackers.register_best_effort(carrier, tracking);
        }
        Ok(FileOutcome::Created(label))
    }
}

/// Overlay incoming fields onto existing, appending a history entry.
fn merge(mut existing: Value, incoming: Value) -> Value {
    let Value::Object(mut existing_obj) = existing.take() else {
        // Existing isn't an object; replace wholesale.
        return with_initial_history(incoming);
    };
    let Value::Object(incoming_obj) = incoming else {
        return Value::Object(existing_obj);
    };

    let history_entry = history_entry_from(&incoming_obj);

    for (k, v) in incoming_obj {
        if k == "history" {
            continue;
        }
        // Preserve the original top-level receivedAt (when the parcel
        // was first filed) even as later updates flow in; each update's
        // date is captured in the per-history entry.
        if k == "receivedAt" && existing_obj.contains_key("receivedAt") {
            continue;
        }
        existing_obj.insert(k, v);
    }

    let history = existing_obj
        .entry("history")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(arr) = history {
        arr.push(history_entry);
    }

    Value::Object(existing_obj)
}

fn with_initial_history(incoming: Value) -> Value {
    let Value::Object(mut obj) = incoming else {
        return incoming;
    };
    let history_entry = history_entry_from(&obj);
    obj.insert("history".to_string(), Value::Array(vec![history_entry]));
    Value::Object(obj)
}

/// Synthesise a public tracking URL for well-known carriers.
///
/// Returns `None` for carriers we don't have a template for; the
/// extractor is welcome to provide a `trackingUrl` of its own for any
/// carrier, which always wins over this fallback.
///
/// URLs are the ones the carrier hands out to customers on their
/// public tracking page; format changes on their side are the failure
/// mode. Keep the list short and only add carriers with a stable,
/// tracking-number-only URL scheme.
fn tracking_url_for(carrier: &str, tracking: &str) -> Option<String> {
    let enc = percent_encoding::utf8_percent_encode(tracking, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    let url = match carrier {
        "royal-mail" => {
            format!("https://www.royalmail.com/track-your-item#/tracking-results/{enc}")
        }
        "dpd" => format!("https://www.dpd.co.uk/service/tracking?parcel={enc}"),
        "postnl" => format!("https://jouw.postnl.nl/track-and-trace/{enc}-NL-NL"),
        "evri" | "hermes" => format!("https://www.evri.com/track/parcel/{enc}"),
        "parcelforce" => format!("https://www.parcelforce.com/track-trace?trackNumber={enc}"),
        "yodel" => format!("https://www.yodel.co.uk/tracking/{enc}"),
        "dhl" => format!("https://www.dhl.com/en/express/tracking.html?AWB={enc}"),
        "ups" => format!("https://www.ups.com/track?tracknum={enc}"),
        "fedex" => format!("https://www.fedex.com/fedextrack/?trknbr={enc}"),
        "usps" => format!("https://tools.usps.com/go/TrackConfirmAction?tLabels={enc}"),
        "amazon" => format!("https://www.amazon.co.uk/progress-tracker/package/{enc}"),
        _ => return None,
    };
    Some(url)
}

fn history_entry_from(obj: &Map<String, Value>) -> Value {
    let mut entry = Map::new();
    entry.insert(
        "seen_at".to_string(),
        Value::String(Utc::now().to_rfc3339()),
    );
    for key in [
        "receivedAt",
        "deliveryStatus",
        "expectedArrivalUntil",
        "expectedArrivalFrom",
        "actualDeliveryTime",
    ] {
        if let Some(v) = obj.get(key) {
            entry.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_url_synthesised_for_known_carrier() {
        assert!(
            tracking_url_for("royal-mail", "TQ123GB")
                .unwrap()
                .contains("TQ123GB")
        );
        assert!(
            tracking_url_for("dpd", "15500806388448")
                .unwrap()
                .contains("15500806388448")
        );
    }

    #[test]
    fn tracking_url_none_for_unknown_carrier() {
        assert!(tracking_url_for("moon-post", "X").is_none());
    }

    #[test]
    fn tracking_url_percent_encodes_special_chars() {
        let url = tracking_url_for("royal-mail", "AB 12/34").unwrap();
        assert!(url.contains("AB%2012%2F34"), "got: {url}");
    }

    #[test]
    fn file_parcel_stamps_tracking_url_for_known_carrier() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in.json");
        std::fs::write(
            &src,
            br#"{"trackingNumber":"TQ123GB","deliveryStatus":"OnItsWay",
                 "provider":{"@id":"royal-mail","name":"Royal Mail"}}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");
        std::fs::create_dir_all(&dir).unwrap();
        file_parcel(&src, &dir, None, None).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("TQ123GB.json")).unwrap())
                .unwrap();
        assert!(v["trackingUrl"].as_str().unwrap().contains("TQ123GB"));
    }

    #[test]
    fn file_parcel_leaves_extractor_tracking_url_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in.json");
        std::fs::write(
            &src,
            br#"{"trackingNumber":"TQ123GB","provider":{"@id":"royal-mail"},
                 "trackingUrl":"https://example.org/custom"}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");
        std::fs::create_dir_all(&dir).unwrap();
        file_parcel(&src, &dir, None, None).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("TQ123GB.json")).unwrap())
                .unwrap();
        assert_eq!(v["trackingUrl"], "https://example.org/custom");
    }

    #[test]
    fn tracking_from_serde() {
        let p: Parcel = serde_json::from_value(serde_json::json!({
            "trackingNumber": "TQ123GB"
        }))
        .unwrap();
        assert_eq!(p.tracking(), Some("TQ123GB"));
    }

    #[test]
    fn merge_appends_history() {
        let existing = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "OnItsWay",
            "history": [
                {"seen_at": "2024-12-15T10:00:00Z", "deliveryStatus": "OnItsWay"}
            ]
        });
        let incoming = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "OutForDelivery"
        });
        let merged = merge(existing, incoming);
        let obj = merged.as_object().unwrap();
        assert_eq!(obj.get("deliveryStatus").unwrap(), "OutForDelivery");
        let history = obj.get("history").unwrap().as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].get("deliveryStatus").unwrap(), "OutForDelivery");
    }

    #[test]
    fn file_parcel_stamps_received_at_and_preserves_it_on_update() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("in.json");
        std::fs::write(
            &src,
            br#"{"trackingNumber":"XYZ","deliveryStatus":"Scheduled"}"#,
        )
        .unwrap();
        let dir = tmp.path().join("out");
        std::fs::create_dir_all(&dir).unwrap();
        // First filing: 2024-11-01T00:00:00Z
        file_parcel(&src, &dir, None, Some(1730419200)).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("XYZ.json")).unwrap()).unwrap();
        assert_eq!(v["receivedAt"], "2024-11-01T00:00:00Z");
        let history = v["history"].as_array().unwrap();
        assert_eq!(history[0]["receivedAt"], "2024-11-01T00:00:00Z");

        // Second filing (update): 2024-11-15T00:00:00Z. Top-level
        // receivedAt should stay at the original date; the new history
        // entry should carry the update's date.
        std::fs::write(
            &src,
            br#"{"trackingNumber":"XYZ","deliveryStatus":"OutForDelivery"}"#,
        )
        .unwrap();
        file_parcel(&src, &dir, None, Some(1731628800)).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("XYZ.json")).unwrap()).unwrap();
        assert_eq!(
            v["receivedAt"], "2024-11-01T00:00:00Z",
            "top-level receivedAt should be preserved"
        );
        let history = v["history"].as_array().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1]["receivedAt"], "2024-11-15T00:00:00Z");
    }
}
