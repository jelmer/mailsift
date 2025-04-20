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

    let incoming: Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing parcel JSON {}", src.display()))?;

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

fn history_entry_from(obj: &Map<String, Value>) -> Value {
    let mut entry = Map::new();
    entry.insert(
        "seen_at".to_string(),
        Value::String(Utc::now().to_rfc3339()),
    );
    for key in [
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
}
