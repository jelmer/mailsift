//! Tracker-registration fan-out.
//!
//! When the parcel target writes a brand-new `.parcel.json` record (a
//! tracking number we haven't seen before), each configured tracker
//! sink gets the chance to register that tracking number with its
//! upstream service so the service starts polling the carrier for
//! status updates. Currently shipping:
//!
//! - [`super::karrio::KarrioClient`]
//! - [`super::seventeentrack::SeventeenTrackClient`]
//!
//! Adding a new service is one new module that implements
//! [`TrackerSink`] plus a `register()` call in `main`.

use tracing::warn;

/// Something that can register a tracking number with a remote
/// service.
pub trait TrackerSink: Send + Sync {
    /// Display name for log messages.
    fn name(&self) -> &str;

    /// Register `tracking_number` for the given `carrier_id`. The
    /// implementation is expected to be idempotent (re-registering the
    /// same `(carrier, tracking)` pair returns success).
    ///
    /// Errors are returned for the caller's logging; they shouldn't
    /// abort anything else.
    fn register(&self, carrier_id: &str, tracking_number: &str) -> anyhow::Result<()>;
}

/// Holds zero-or-more tracker sinks. Wraps them so the parcel target
/// fans out to all of them on creation of a new parcel record.
#[derive(Default)]
pub struct Trackers {
    sinks: Vec<Box<dyn TrackerSink>>,
}

impl Trackers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push<T: TrackerSink + 'static>(&mut self, sink: T) {
        self.sinks.push(Box::new(sink));
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    /// Fan out to every configured sink. Errors are logged at WARN;
    /// one tracker being down does not stop the others or affect the
    /// on-disk parcel record.
    pub fn register_best_effort(&self, carrier_id: &str, tracking_number: &str) {
        for sink in &self.sinks {
            if let Err(e) = sink.register(carrier_id, tracking_number) {
                warn!(
                    sink = sink.name(),
                    tracking = %tracking_number,
                    carrier = %carrier_id,
                    error = %e,
                    "tracker registration failed; on-disk parcel record is unaffected"
                );
            }
        }
    }
}
