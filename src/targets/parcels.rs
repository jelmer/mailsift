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
use chrono::{DateTime, Utc};
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

/// Fields describing where a parcel is right now, as opposed to what
/// it is. Only a mail newer than everything already recorded may
/// overwrite these; the rest merge unconditionally.
const STATE_FIELDS: [&str; 5] = [
    "deliveryStatus",
    "expectedArrivalFrom",
    "expectedArrivalUntil",
    "actualDeliveryTime",
    "trackingUrl",
];

/// Overlay incoming fields onto existing, appending a history entry.
///
/// A mailbox is not processed in date order -- a rescan, a re-filed
/// folder or an IMAP scan that walks messages by UID can all hand us
/// an older mail after a newer one. Applying every incoming field
/// blindly then rolls the parcel's state backwards, leaving a
/// delivered parcel claiming to be out for delivery. So the state
/// fields above are only taken from a mail at least as new as the
/// newest one already merged; everything else still overlays, and the
/// history records the mail either way.
fn merge(mut existing: Value, incoming: Value) -> Value {
    let Value::Object(mut existing_obj) = existing.take() else {
        // Existing isn't an object; replace wholesale.
        return with_initial_history(incoming);
    };
    let Value::Object(incoming_obj) = incoming else {
        return Value::Object(existing_obj);
    };

    let history_entry = history_entry_from(&incoming_obj);
    let incoming_date = received_at_of(&incoming_obj);
    let newest_known = newest_received_at(&existing_obj);
    let is_stale = match (incoming_date, newest_known) {
        (Some(incoming), Some(newest)) => incoming < newest,
        // Undated mail can't be ordered by date. Fall back on the one
        // thing we know regardless: a parcel that has been delivered
        // or returned does not go back to being in transit. Not every
        // extractor sets `receivedAt`, and those records hit this path
        // exclusively.
        _ => is_final_status(&existing_obj) && !is_final_status(&incoming_obj),
    };

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
        if is_stale && STATE_FIELDS.contains(&k.as_str()) {
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

/// Whether a record's `deliveryStatus` is a terminal one. Matches the
/// schema.org `DeliveryEvent` and `OrderStatus` spellings extractors
/// emit, ignoring case and separators.
fn is_final_status(obj: &Map<String, Value>) -> bool {
    let Some(status) = obj.get("deliveryStatus").and_then(Value::as_str) else {
        return false;
    };
    let normalised: String = status
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    matches!(
        normalised.as_str(),
        "delivered" | "orderdelivered" | "returned" | "orderreturned" | "returnedtosender"
    )
}

/// The `receivedAt` of a single record or history entry, as a
/// comparable timestamp.
fn received_at_of(obj: &Map<String, Value>) -> Option<DateTime<Utc>> {
    let raw = obj.get("receivedAt")?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// The newest mail date already merged into a record: the top-level
/// `receivedAt` and every history entry's, whichever is latest.
fn newest_received_at(obj: &Map<String, Value>) -> Option<DateTime<Utc>> {
    let from_history = obj
        .get("history")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(received_at_of);
    received_at_of(obj).into_iter().chain(from_history).max()
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
    fn older_mail_does_not_overwrite_a_newer_status() {
        // Mailboxes are not processed in date order, so a delivery
        // notification can be filed before the "out for delivery" mail
        // that preceded it. The newer mail must win regardless.
        let existing = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "Delivered",
            "receivedAt": "2024-12-20T15:00:00Z",
            "history": [
                {"seen_at": "2024-12-21T10:00:00Z", "receivedAt": "2024-12-20T15:00:00Z",
                 "deliveryStatus": "Delivered"}
            ]
        });
        let incoming = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "OutForDelivery",
            "receivedAt": "2024-12-20T08:00:00Z"
        });
        let merged = merge(existing, incoming);
        let obj = merged.as_object().unwrap();
        assert_eq!(
            obj.get("deliveryStatus").unwrap(),
            "Delivered",
            "an older mail must not un-deliver a parcel"
        );
        // The older mail is still recorded in the history.
        assert_eq!(obj.get("history").unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn newer_mail_updates_the_status() {
        let existing = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "OutForDelivery",
            "receivedAt": "2024-12-20T08:00:00Z",
            "history": [
                {"seen_at": "2024-12-20T09:00:00Z", "receivedAt": "2024-12-20T08:00:00Z",
                 "deliveryStatus": "OutForDelivery"}
            ]
        });
        let incoming = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "Delivered",
            "receivedAt": "2024-12-20T15:00:00Z"
        });
        let merged = merge(existing, incoming);
        assert_eq!(
            merged.as_object().unwrap().get("deliveryStatus").unwrap(),
            "Delivered"
        );
    }

    #[test]
    fn a_stale_mail_still_contributes_non_state_fields() {
        // Only the state fields are held back; a description the older
        // mail carries is still worth having.
        let existing = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "Delivered",
            "receivedAt": "2024-12-20T15:00:00Z"
        });
        let incoming = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "OnItsWay",
            "description": "A book",
            "receivedAt": "2024-12-19T08:00:00Z"
        });
        let obj = merge(existing, incoming);
        let obj = obj.as_object().unwrap();
        assert_eq!(obj.get("deliveryStatus").unwrap(), "Delivered");
        assert_eq!(obj.get("description").unwrap(), "A book");
    }

    #[test]
    fn ordering_considers_history_dates_not_just_the_top_level() {
        // The top-level receivedAt stays at the first mail's date, so
        // ordering has to look at the history to find the newest.
        let existing = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "Delivered",
            "receivedAt": "2024-12-01T08:00:00Z",
            "history": [
                {"receivedAt": "2024-12-01T08:00:00Z", "deliveryStatus": "OnItsWay"},
                {"receivedAt": "2024-12-20T15:00:00Z", "deliveryStatus": "Delivered"}
            ]
        });
        let incoming = serde_json::json!({
            "trackingNumber": "X",
            "deliveryStatus": "OutForDelivery",
            "receivedAt": "2024-12-20T08:00:00Z"
        });
        let merged = merge(existing, incoming);
        assert_eq!(
            merged.as_object().unwrap().get("deliveryStatus").unwrap(),
            "Delivered"
        );
    }

    #[test]
    fn same_timestamp_still_applies() {
        // Equal dates are not stale: two mails in the same second
        // should behave as before.
        let existing = serde_json::json!({
            "trackingNumber": "X", "deliveryStatus": "OnItsWay",
            "receivedAt": "2024-12-20T08:00:00Z"
        });
        let incoming = serde_json::json!({
            "trackingNumber": "X", "deliveryStatus": "Delivered",
            "receivedAt": "2024-12-20T08:00:00Z"
        });
        let merged = merge(existing, incoming);
        assert_eq!(
            merged.as_object().unwrap().get("deliveryStatus").unwrap(),
            "Delivered"
        );
    }

    #[test]
    fn undated_mail_cannot_undeliver_a_parcel() {
        // Not every extractor sets receivedAt. Without dates we still
        // refuse to walk a terminal state backwards.
        let existing = serde_json::json!({"trackingNumber": "X", "deliveryStatus": "Delivered"});
        let incoming = serde_json::json!({"trackingNumber": "X", "deliveryStatus": "OnItsWay"});
        let merged = merge(existing, incoming);
        assert_eq!(
            merged.as_object().unwrap().get("deliveryStatus").unwrap(),
            "Delivered"
        );
    }

    #[test]
    fn undated_mail_still_advances_a_parcel() {
        // The guard only blocks moving away from a terminal state; an
        // ordinary progression still applies.
        let existing = serde_json::json!({"trackingNumber": "X", "deliveryStatus": "OnItsWay"});
        let incoming = serde_json::json!({"trackingNumber": "X", "deliveryStatus": "Delivered"});
        let merged = merge(existing, incoming);
        assert_eq!(
            merged.as_object().unwrap().get("deliveryStatus").unwrap(),
            "Delivered"
        );
    }

    #[test]
    fn a_returned_parcel_is_also_terminal() {
        let existing =
            serde_json::json!({"trackingNumber": "X", "deliveryStatus": "OrderReturned"});
        let incoming =
            serde_json::json!({"trackingNumber": "X", "deliveryStatus": "OutForDelivery"});
        let merged = merge(existing, incoming);
        assert_eq!(
            merged.as_object().unwrap().get("deliveryStatus").unwrap(),
            "OrderReturned"
        );
    }

    #[test]
    fn timezones_are_compared_correctly() {
        // 09:00+02:00 is 07:00Z, older than 08:00Z despite the larger
        // wall-clock time.
        let existing = serde_json::json!({
            "trackingNumber": "X", "deliveryStatus": "Delivered",
            "receivedAt": "2024-12-20T08:00:00Z"
        });
        let incoming = serde_json::json!({
            "trackingNumber": "X", "deliveryStatus": "OnItsWay",
            "receivedAt": "2024-12-20T09:00:00+02:00"
        });
        let merged = merge(existing, incoming);
        assert_eq!(
            merged.as_object().unwrap().get("deliveryStatus").unwrap(),
            "Delivered"
        );
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

#[cfg(test)]
mod real_data_tests {
    use super::*;

    /// Replay a record's history through `merge` in the order mailsift
    /// originally processed it (by `seen_at`), and report the status
    /// the record ends up with.
    fn replay(history: &[Value]) -> String {
        let mut record = Value::Object(Map::new());
        let mut first = true;
        for entry in history {
            let obj = entry.as_object().unwrap();
            let mut incoming = Map::new();
            incoming.insert("trackingNumber".into(), Value::String("X".into()));
            for key in ["deliveryStatus", "receivedAt"] {
                if let Some(v) = obj.get(key) {
                    incoming.insert(key.to_string(), v.clone());
                }
            }
            let incoming = Value::Object(incoming);
            record = if first {
                first = false;
                with_initial_history(incoming)
            } else {
                merge(record, incoming)
            };
        }
        record
            .get("deliveryStatus")
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_string()
    }

    #[test]
    fn undated_delivery_survives_a_later_older_mail() {
        // Shape taken from a real record: mailsift saw the delivery
        // notification first, then the earlier "out for delivery" one,
        // and neither carries a receivedAt.
        let history = vec![
            serde_json::json!({"deliveryStatus": "Delivered"}),
            serde_json::json!({"deliveryStatus": "OutForDelivery"}),
        ];
        assert_eq!(replay(&history), "Delivered");
    }

    #[test]
    fn dated_out_of_order_history_settles_on_the_newest() {
        let history = vec![
            serde_json::json!({"deliveryStatus": "OnItsWay", "receivedAt": "2025-11-20T09:00:00Z"}),
            serde_json::json!({"deliveryStatus": "Delivered", "receivedAt": "2025-11-24T14:00:00Z"}),
            serde_json::json!({"deliveryStatus": "OutForDelivery", "receivedAt": "2025-11-24T08:00:00Z"}),
        ];
        assert_eq!(replay(&history), "Delivered");
    }
}
